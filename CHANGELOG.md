# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## [1.3.0] - 2026-10-28

### Added
- `[notify]` (disabled by default): announce the end of each folder's batch, so the
  infrastructure can rescan what changed (Nextcloud needs `occ files:scan`; the
  container cannot run it). Two optional sinks:
  - `notify.dir`: `<dir>/<folder>.json`, overwritten at each batch and written atomically
    (temporary file in `<dir>/.tmp/`, then `rename`), so a reader never sees a truncated
    JSON nor is woken early by a temporary. Carries a `seq` that grows per folder (kept in
    `state_dir/notify.json`, so it survives restarts), the counts, and the directories
    touched — relative to the library (`library_dirs`) and to the input (`input_dirs`,
    covering moved files and removed duplicate sources).
  - `notify.webhook_url`: the same JSON in a `POST` (`http://` only, one synchronous
    attempt bounded by a 5 s deadline for the whole exchange, name lookup included; a
    receiver must answer `2xx` at once). The health probe gets the same global deadline.
  One event per folder; none in `--dry-run`, none for a batch that changed nothing.
  Uploads trickle in and are organized as they arrive, so the announcement is **held**
  and merged (files and touched directories) until the input has been quiet for
  `notify.quiet_secs` (default 60), but never longer than `notify.max_wait_secs`
  (default 600) after the first held change, so a continuous flow cannot postpone it
  forever. Organizing itself is never delayed. Held changes are persisted in
  `state_dir/notify.json`; `quiet_secs = 0` announces after every batch. A failure is a `warn`, never a failed batch. `recover`
  announces what it publishes; so does a replay at startup, including files published
  just before a crash.
- `/healthz`: a `notify` block (only when configured) with the last `seq`, date and counts
  per folder, and `last_error`; `status` is `degraded` while the last announcement failed.

### Changed
- Internal, no behaviour change: `scanner.rs` is split by phase (`scanner/`), the HTTP
  probe and the webhook share a small `http` module (still no dependency), and the
  journal, the caches and the event share `fsops::write_atomic`. Caches are now fsynced
  before the rename, like the journal already was.
- `scripts/smoke-test.sh` enables `[notify]` and checks the announcement end to end: the
  event file (content, mode, no leftover temporary), nothing for `--dry-run` or a kept
  duplicate, and the last `seq` restored in `/healthz` after a restart.

---

## [1.2.0] - 2026-10-27

### Added
- `dedup.delete_source_duplicates` (default `false`): remove the source of a duplicate once
  it is proven byte-identical to the file already in the library. The check is a fresh
  byte-for-byte read of both files at removal time (not the cached BLAKE3 index); a
  duplicate inside the same batch waits for the next scan, and any doubt (different
  content, source modified since planning, symlink, same inode, read error) keeps the
  source. Runs in phase 5; `--dry-run` only logs. Requires `dedup.enabled` and
  `dedup.check_existing`. Duplicates remembered before enabling it are not re-examined:
  delete `duplicates.json` once.

---

## [1.1.2] - 2026-10-22

### Added
- The image now carries a `dev.dozzle.icon` label (folder with a photo and a video card)
  so Dozzle shows a media-organizer logo. Source: `docker/dozzle-icon.svg`.

---

## [1.1.1] - 2026-10-22

First production deployment fixes.

### Fixed
- Commits into an existing library no longer fail with `Operation not permitted`. The
  directory chain is now chmod-ed only where the mode differs, and a refused `chmod`
  (directory owned by another uid) is logged as a warning instead of failing the batch.
- `--dry-run` in daemon mode reported the whole backlog on every poll. Each plan is now
  reported once per process, then only when it changes.

### Added
- `lowercase_ext` (per folder, default `false`): render `{ext}` in lowercase.

### Changed
- Log timestamps use the local zone (`TZ`) and read `2026-10-22 20:25:42`.
- **JSON log lines are flat**: `fields.message` and the other `fields.*` properties are now
  top-level keys, and `target` is gone. Update any query or alert that reads the old keys.
- The `text` log format is compact (no module path) and colors only on a terminal.

---

## [1.1.0] - 2026-10-22

Inputs and libraries are now declared by name, one volume each, and the work area is
private to the service.

### Changed
- **Breaking:** `output` is renamed `library`.
- **Breaking:** `[staging] dir` is removed; the staging area moves out of the library into a
  private work directory (`work_dir`, default `<state_dir>/work`, one subdirectory per
  folder). Each `[[folders]]` must have a unique `name`.
- Publishing is now two steps: each file is placed next to its destination under a hidden
  temporary name (`.<name>.mo-part`, never indexed by library scanners such as Immich), then
  every file is linked to its final name atomically. A file appears complete or not at all,
  and an outside writer's file is never overwritten.
- The same-filesystem check between staging and output is gone: separate mounts are
  supported (files are copied across them instead of hard-linked).

### Added
- `inputs_root` / `libraries_root`: a bare `input` or `library` name is resolved under
  `/inputs/<name>` or `/libraries/<name>`, so a compose file mounts each folder separately.
- `work_dir` setting.

---

## [1.0.0] - 2026-10-22

First release of `media-organizer`, a rewrite of `syno-media-organizer` as an arm64
container for a Raspberry Pi 5 that mounts the NAS over NFS.

### Added
- **Transactional pipeline**: inventory, planning, resolution, preparation, commit.
  Nothing is written to `output` before the final burst of `rename()` calls.
- **Staging area** on the same filesystem as `output`, with a crash-safe journal that is
  replayed at startup and on every scan; `recover` subcommand.
- **Duplicate detection by name and content** (BLAKE3, size pre-filter, persistent hash
  index). A duplicate is left in place and remembered, never deleted.
- **Date fallback chain** ending with the file's modification time, so no file stays
  stuck in the input folder. The mtime is logged at `warn` and counted separately.
- `GET /healthz` (JSON, 503 when no progress) and `GET /livez`; `healthcheck` subcommand
  for the container `HEALTHCHECK`.
- `{camera}` pattern token, backed by the EXIF `Model` tag.
- `validate-config`, `--dry-run` (full plan, no writes), `--once`, `MO_*` environment
  variables.
- Parallel planning and preparation (`workers`), `stability_secs` to skip files still
  being uploaded.
- Configurable `file_mode` / `dir_mode`, written as octal strings.
- arm64 image published to `ghcr.io/jordanatdown/media-organizer`, tagged `vX.Y.Z` only.

### Changed
- EXIF date injection now happens on the staged copy: the source file is never rewritten.
- `output` inside a recursive `input` is rejected at validation time.
- Unknown configuration keys are an error.
- Directories pruned during the walk instead of being filtered file by file.
- Release build tuned for speed, not size. All dependencies updated.

### Removed
- `{counter}` token (it always rendered `0000`); rejected with a migration hint.
- DSM packaging (`.spk`, GPG signing, `packages.json`), the musl/Zig cross toolchain, the
  `clock_gettime` override, file logging.

[Unreleased]: https://github.com/JordanAtDown/media-organizer/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/JordanAtDown/media-organizer/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/JordanAtDown/media-organizer/releases/tag/v1.0.0
