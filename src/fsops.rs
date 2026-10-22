//! Filesystem primitives shared by the staging and commit phases.

use crate::config::Mode;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

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
pub fn ensure_dir_chain(output: &Path, dir: &Path, mode: Option<Mode>) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if mode.is_none() {
        return Ok(());
    }
    let mut current = dir;
    loop {
        apply_mode(current, mode)?;
        if current == output {
            return Ok(());
        }
        match current.parent() {
            Some(parent) if parent.starts_with(output) => current = parent,
            _ => return Ok(()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

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
}
