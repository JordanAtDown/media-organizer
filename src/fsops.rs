//! Filesystem primitives shared by the staging and commit phases.

use crate::config::Mode;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tracing::warn;

/// How a file reached the staging area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Materialized {
    /// Hard link: a free metadata operation, the inode is shared with the source.
    Linked,
    /// Full copy: independent inode, safe to modify.
    Copied,
}

/// `chmod` only when a mode is configured. `None` means "do not touch".
///
/// `set_permissions` ignores the umask, so the result is the same on every machine.
pub fn apply_mode(path: &Path, mode: Option<Mode>) -> io::Result<()> {
    match mode {
        Some(Mode(m)) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)),
        None => Ok(()),
    }
}

/// Create `dir` and open **every** directory from `output` down to it.
///
/// `create_dir_all` may create several missing levels at once (year *and* month),
/// all under the process umask; on an `all_squash` NFS export that leaves them
/// unreadable by the identity Nextcloud serves them with. So the whole chain is
/// chmod-ed, not just the deepest directory. `dir` must be `output` or below it.
///
/// Only directories whose mode differs are touched: `chmod` is reserved to the
/// owner, and the historical levels of a library belong to other uids.
pub fn ensure_dir_chain(output: &Path, dir: &Path, mode: Option<Mode>) -> io::Result<()> {
    ensure_dir_chain_with(output, dir, mode, &|path, Mode(m)| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(m))
    })
}

/// [`ensure_dir_chain`] with the `chmod` injected, so a refusal (a directory
/// owned by someone else) can be simulated without root.
fn ensure_dir_chain_with(
    output: &Path,
    dir: &Path,
    mode: Option<Mode>,
    chmod: &dyn Fn(&Path, Mode) -> io::Result<()>,
) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let Some(wanted) = mode else {
        return Ok(());
    };
    let mut current = dir;
    loop {
        open_dir(current, wanted, chmod)?;
        if current == output {
            return Ok(());
        }
        match current.parent() {
            Some(parent) if parent.starts_with(output) => current = parent,
            _ => return Ok(()),
        }
    }
}

/// Bring one directory to `wanted`. A refused `chmod` is tolerated: the
/// directory is not ours, and in the common case it is already usable.
fn open_dir(
    path: &Path,
    wanted: Mode,
    chmod: &dyn Fn(&Path, Mode) -> io::Result<()>,
) -> io::Result<()> {
    let actual = std::fs::metadata(path)?.permissions().mode() & 0o7777;
    if actual == wanted.0 {
        return Ok(());
    }
    match chmod(path, wanted) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            warn!(
                path = %path.display(), actual = format_args!("{actual:04o}"),
                wanted = format_args!("{:04o}", wanted.0),
                "cannot chmod directory (not owned by the service); leaving it as is"
            );
            Ok(())
        }
        other => other,
    }
}

/// Hard-link `src` to `dst`, falling back to a copy when linking is impossible
/// (different filesystems, an export without hard links).
pub fn link_or_copy(src: &Path, dst: &Path) -> io::Result<Materialized> {
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(Materialized::Linked),
        Err(_) => {
            std::fs::copy(src, dst)?;
            Ok(Materialized::Copied)
        }
    }
}

/// Copy `src` to `dst` unconditionally, for when the copy is about to be modified
/// (EXIF injection) and must not share an inode with the source.
pub fn copy_file(src: &Path, dst: &Path) -> io::Result<Materialized> {
    std::fs::copy(src, dst)?;
    Ok(Materialized::Copied)
}

/// Why [`remove_if_identical`] left the source alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// Not two distinct regular files (missing target, symlink, same inode).
    NotComparable,
    /// Sizes or contents differ.
    Different,
    /// The source changed while it was being compared.
    Changed,
}

/// Delete `source` only if it is provably the same bytes as `target`.
///
/// This is a fresh check, independent of every cache: both files are read in
/// full right now, and `source` must still have the size and mtime the scan
/// planned with (`source_mtime`) before and after the comparison. Any doubt or
/// read error keeps the source: `Err` means nothing was removed.
pub fn remove_if_identical(
    source: &Path,
    target: &Path,
    source_mtime: u64,
) -> io::Result<Result<(), Kept>> {
    let (src_meta, dst_meta) = (
        std::fs::symlink_metadata(source)?,
        std::fs::symlink_metadata(target)?,
    );
    if !src_meta.is_file() || !dst_meta.is_file() {
        return Ok(Err(Kept::NotComparable));
    }
    if (src_meta.dev(), src_meta.ino()) == (dst_meta.dev(), dst_meta.ino()) {
        return Ok(Err(Kept::NotComparable));
    }
    if src_meta.len() != dst_meta.len() {
        return Ok(Err(Kept::Different));
    }
    if src_meta.mtime() as u64 != source_mtime {
        return Ok(Err(Kept::Changed));
    }
    if !same_bytes(File::open(source)?, File::open(target)?)? {
        return Ok(Err(Kept::Different));
    }
    let after = std::fs::symlink_metadata(source)?;
    if after.len() != src_meta.len() || after.modified()? != src_meta.modified()? {
        return Ok(Err(Kept::Changed));
    }
    std::fs::remove_file(source)?;
    Ok(Ok(()))
}

fn same_bytes(mut a: File, mut b: File) -> io::Result<bool> {
    const CHUNK: usize = 256 * 1024;
    let (mut buf_a, mut buf_b) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let (n_a, n_b) = (fill(&mut a, &mut buf_a)?, fill(&mut b, &mut buf_b)?);
        if n_a != n_b || buf_a[..n_a] != buf_b[..n_b] {
            return Ok(false);
        }
        if n_a == 0 {
            return Ok(true);
        }
    }
}

/// Read until `buf` is full or EOF, so both sides are compared on equal chunks.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn apply_mode_sets_exactly_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        apply_mode(&f, Some(Mode(0o666))).unwrap();
        assert_eq!(mode_of(&f), 0o666);
    }

    #[test]
    fn apply_mode_none_leaves_permissions_alone() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        apply_mode(&f, None).unwrap();
        assert_eq!(mode_of(&f), 0o640);
    }

    #[test]
    fn dir_chain_is_opened_down_to_output() {
        let out = tempfile::tempdir().unwrap();
        let leaf = out.path().join("2025").join("06");
        ensure_dir_chain(out.path(), &leaf, Some(Mode(0o777))).unwrap();
        assert_eq!(mode_of(&leaf), 0o777);
        assert_eq!(mode_of(&out.path().join("2025")), 0o777);
        assert_eq!(mode_of(out.path()), 0o777);
    }

    /// A `chmod` that records its calls and delegates to the real one.
    fn recording_chmod(
        calls: &std::cell::RefCell<Vec<PathBuf>>,
    ) -> impl Fn(&Path, Mode) -> io::Result<()> + '_ {
        move |path, Mode(m)| {
            calls.borrow_mut().push(path.to_path_buf());
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(m))
        }
    }

    fn set_mode(p: &Path, m: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

    #[test]
    fn dir_chain_skips_chmod_on_directories_already_at_the_mode() {
        let out = tempfile::tempdir().unwrap();
        let leaf = out.path().join("2025").join("06");
        std::fs::create_dir_all(&leaf).unwrap();
        for d in [out.path(), &out.path().join("2025"), &leaf] {
            set_mode(d, 0o777);
        }
        let calls = std::cell::RefCell::new(Vec::new());
        ensure_dir_chain_with(
            out.path(),
            &leaf,
            Some(Mode(0o777)),
            &recording_chmod(&calls),
        )
        .unwrap();
        assert!(
            calls.borrow().is_empty(),
            "chmod called on {:?}",
            calls.borrow()
        );
    }

    #[test]
    fn dir_chain_fixes_only_the_levels_that_differ() {
        let out = tempfile::tempdir().unwrap();
        let (year, leaf) = (out.path().join("2025"), out.path().join("2025").join("06"));
        std::fs::create_dir_all(&leaf).unwrap();
        set_mode(out.path(), 0o777);
        set_mode(&year, 0o755);
        set_mode(&leaf, 0o777);
        let calls = std::cell::RefCell::new(Vec::new());
        ensure_dir_chain_with(
            out.path(),
            &leaf,
            Some(Mode(0o777)),
            &recording_chmod(&calls),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec![year.clone()]);
        assert_eq!(mode_of(&year), 0o777);
    }

    #[test]
    fn dir_chain_chmods_every_new_level() {
        let out = tempfile::tempdir().unwrap();
        set_mode(out.path(), 0o777);
        let leaf = out.path().join("2025").join("06");
        let calls = std::cell::RefCell::new(Vec::new());
        ensure_dir_chain_with(
            out.path(),
            &leaf,
            Some(Mode(0o777)),
            &recording_chmod(&calls),
        )
        .unwrap();
        // The new year and month differ (umask), the already-open root does not.
        assert_eq!(calls.borrow().len(), 2);
        assert_eq!(mode_of(&leaf), 0o777);
        assert_eq!(mode_of(&out.path().join("2025")), 0o777);
    }

    #[test]
    fn dir_chain_tolerates_a_refused_chmod() {
        let out = tempfile::tempdir().unwrap();
        let leaf = out.path().join("2025").join("06");
        std::fs::create_dir_all(&leaf).unwrap();
        set_mode(&leaf, 0o755);
        let refuse = |_: &Path, _: Mode| Err(io::Error::from_raw_os_error(1)); // EPERM
        ensure_dir_chain_with(out.path(), &leaf, Some(Mode(0o777)), &refuse).unwrap();
        assert_eq!(mode_of(&leaf), 0o755);
    }

    #[test]
    fn dir_chain_still_fails_on_other_chmod_errors() {
        let out = tempfile::tempdir().unwrap();
        set_mode(out.path(), 0o755);
        let broken = |_: &Path, _: Mode| Err(io::Error::from_raw_os_error(5)); // EIO
        let err =
            ensure_dir_chain_with(out.path(), out.path(), Some(Mode(0o777)), &broken).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(5));
    }

    #[test]
    fn dir_chain_never_climbs_above_output() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        ensure_dir_chain(&out, &out.join("a"), Some(Mode(0o777))).unwrap();
        assert_eq!(mode_of(root.path()), 0o750);
    }

    #[test]
    fn dir_chain_without_mode_only_creates() {
        let out = tempfile::tempdir().unwrap();
        let before = mode_of(out.path());
        let leaf = out.path().join("a").join("b");
        ensure_dir_chain(out.path(), &leaf, None).unwrap();
        assert!(leaf.is_dir());
        assert_eq!(mode_of(out.path()), before);
    }

    #[test]
    fn link_shares_the_inode() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, b"data").unwrap();
        assert_eq!(link_or_copy(&src, &dst).unwrap(), Materialized::Linked);
        assert_eq!(std::fs::metadata(&src).unwrap().nlink(), 2);
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
    }

    #[test]
    fn copy_has_its_own_inode() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, b"data").unwrap();
        assert_eq!(copy_file(&src, &dst).unwrap(), Materialized::Copied);
        assert_eq!(std::fs::metadata(&src).unwrap().nlink(), 1);
    }

    #[test]
    fn link_falls_back_to_copy_when_destination_dir_differs_in_fs() {
        // /proc rejects hard links from outside; the copy would fail too, so this
        // only asserts the error is surfaced rather than swallowed.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::write(&src, b"x").unwrap();
        assert!(link_or_copy(&src, Path::new("/proc/nope")).is_err());
    }

    // -- remove_if_identical ---------------------------------------------------

    fn pair(a: &[u8], b: &[u8]) -> (tempfile::TempDir, PathBuf, PathBuf, u64) {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, a).unwrap();
        std::fs::write(&dst, b).unwrap();
        let mtime = std::fs::metadata(&src).unwrap().mtime() as u64;
        (dir, src, dst, mtime)
    }

    #[test]
    fn identical_source_is_removed() {
        let (_d, src, dst, mtime) = pair(b"same bytes", b"same bytes");
        assert_eq!(remove_if_identical(&src, &dst, mtime).unwrap(), Ok(()));
        assert!(!src.exists() && dst.exists());
    }

    #[test]
    fn identical_large_source_is_removed() {
        let big = vec![7u8; 600 * 1024];
        let (_d, src, dst, mtime) = pair(&big, &big);
        assert_eq!(remove_if_identical(&src, &dst, mtime).unwrap(), Ok(()));
        assert!(!src.exists());
    }

    #[test]
    fn same_size_different_content_is_kept() {
        let (_d, src, dst, mtime) = pair(b"aaaa", b"aaab");
        assert_eq!(
            remove_if_identical(&src, &dst, mtime).unwrap(),
            Err(Kept::Different)
        );
        assert!(src.exists());
    }

    #[test]
    fn a_difference_in_a_late_chunk_is_found() {
        let mut other = vec![7u8; 600 * 1024];
        let one = other.clone();
        *other.last_mut().unwrap() = 8;
        let (_d, src, dst, mtime) = pair(&one, &other);
        assert_eq!(
            remove_if_identical(&src, &dst, mtime).unwrap(),
            Err(Kept::Different)
        );
        assert!(src.exists());
    }

    #[test]
    fn different_size_is_kept() {
        let (_d, src, dst, mtime) = pair(b"short", b"shorter");
        assert_eq!(
            remove_if_identical(&src, &dst, mtime).unwrap(),
            Err(Kept::Different)
        );
        assert!(src.exists());
    }

    #[test]
    fn missing_target_is_an_error_and_keeps_the_source() {
        let (_d, src, dst, mtime) = pair(b"x", b"x");
        std::fs::remove_file(&dst).unwrap();
        assert!(remove_if_identical(&src, &dst, mtime).is_err());
        assert!(src.exists());
    }

    #[test]
    fn a_source_modified_since_the_plan_is_kept() {
        let (_d, src, dst, mtime) = pair(b"same", b"same");
        assert_eq!(
            remove_if_identical(&src, &dst, mtime + 60).unwrap(),
            Err(Kept::Changed)
        );
        assert!(src.exists());
    }

    #[test]
    fn a_hard_link_of_the_target_is_never_removed() {
        let (_d, src, dst, mtime) = pair(b"same", b"same");
        std::fs::remove_file(&src).unwrap();
        std::fs::hard_link(&dst, &src).unwrap();
        assert_eq!(
            remove_if_identical(&src, &dst, mtime).unwrap(),
            Err(Kept::NotComparable)
        );
        assert!(src.exists());
        assert_eq!(
            remove_if_identical(&dst, &dst, mtime).unwrap(),
            Err(Kept::NotComparable)
        );
    }

    #[test]
    fn a_symlink_is_never_followed_or_removed() {
        let (_d, src, dst, mtime) = pair(b"same", b"same");
        let link = src.with_file_name("link");
        std::os::unix::fs::symlink(&dst, &link).unwrap();
        assert_eq!(
            remove_if_identical(&link, &dst, mtime).unwrap(),
            Err(Kept::NotComparable)
        );
        assert_eq!(
            remove_if_identical(&src, &link, mtime).unwrap(),
            Err(Kept::NotComparable)
        );
        assert!(link.exists() && src.exists());
    }
}
