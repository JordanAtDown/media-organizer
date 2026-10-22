# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

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

[Unreleased]: https://github.com/JordanAtDown/media-organizer/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/JordanAtDown/media-organizer/releases/tag/v1.0.0
