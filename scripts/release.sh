#!/usr/bin/env bash
# Manual release helper. Usage: bash scripts/release.sh X.Y.Z
#
# Bumps the version, commits and tags locally, then PRINTS the push command —
# it never pushes. Pushing the tag triggers .github/workflows/release.yml,
# which publishes the arm64 image and the GitHub release.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "Usage: $0 <version>   e.g.  $0 1.2.3" >&2
    exit 1
fi

VERSION="$1"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "ERROR: version must be in semver format X.Y.Z" >&2
    exit 1
fi

cd "$ROOT"

if [[ "$(git branch --show-current)" != "main" ]]; then
    echo "ERROR: releases are cut from main (trunk-based)." >&2
    exit 1
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "ERROR: working tree is not clean. Commit or stash changes first." >&2
    exit 1
fi
if git rev-parse -q --verify "refs/tags/v${VERSION}" >/dev/null; then
    echo "ERROR: tag v${VERSION} already exists." >&2
    exit 1
fi
if ! grep -q "^## \[${VERSION}\]" CHANGELOG.md; then
    echo "ERROR: no '## [${VERSION}]' section in CHANGELOG.md." >&2
    echo "       Move the [Unreleased] notes under it before releasing." >&2
    exit 1
fi

echo "==> Releasing version ${VERSION}"

sed -i "0,/^version = \".*\"/s//version = \"${VERSION}\"/" Cargo.toml
# Refresh Cargo.lock for the new package version, and prove the tree still builds.
cargo check --quiet --all-targets

git add Cargo.toml Cargo.lock
git commit -q -m "chore(release): bump version to ${VERSION}"
git tag -a "v${VERSION}" -m "Release ${VERSION}"

cat <<MSG

==> Version bumped and tagged locally. Nothing was pushed.
    To publish:

        git push origin main v${VERSION}

    The release workflow then builds ghcr.io/jordanatdown/media-organizer:v${VERSION}
    on a native arm64 runner and checks that it can be pulled anonymously.
    If the anonymous-pull step fails, make the GHCR package public (see RELEASING.md).
MSG
