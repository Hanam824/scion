#!/usr/bin/env bash
# Print the next release tag (vMAJOR.MINOR.PATCH).
#
#   ./scripts/next-version.sh        # e.g. v0.1.3
#
# - No release tag yet: v0.1.0
# - Otherwise: the last tag with the patch number + 1, whatever the commits are
#   (v0.1.0 -> v0.1.1 -> ... -> v0.1.9 -> v0.1.10 -> ... -> v0.1.999 -> v0.1.1000).
#   Major and minor change only when someone tags a release by hand (e.g. git tag v0.2.0);
#   the nightly releases then continue from there (v0.2.1, v0.2.2, ...).
#
# Exits 1 if there is nothing to release: no commits since the last tag, or only doc changes
# (see scripts/code-changes.sh for what counts as docs).
set -euo pipefail

cd "$(dirname "$0")/.."

# The newest version tag reachable from HEAD (by history, not by string order, so v0.1.10 beats v0.1.9).
last_tag="$(git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || true)"
if [ -z "$last_tag" ]; then
    echo "v0.1.0"
    exit 0
fi

if [ -z "$(git log --format=%H "$last_tag..HEAD")" ]; then
    echo "no commits since $last_tag, nothing to release" >&2
    exit 1
fi

if [ -z "$(./scripts/code-changes.sh "$last_tag" HEAD)" ]; then
    echo "only docs changed since $last_tag, nothing to release" >&2
    exit 1
fi

IFS=. read -r major minor patch <<< "${last_tag#v}"
echo "v$major.$minor.$((patch + 1))"
