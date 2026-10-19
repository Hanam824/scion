#!/usr/bin/env bash
# List the files that changed between two revisions, leaving out docs. Empty output = docs-only change.
#
#   ./scripts/code-changes.sh v0.1.3 HEAD          # since a release (used by next-version.sh)
#   ./scripts/code-changes.sh origin/main...HEAD   # what a pull request changes (used by CI)
#
# Arguments go straight to `git diff --name-only`. Docs are any Markdown file (in any folder),
# doc/, docs/ and LICENSE. This is the one place that list lives.
set -euo pipefail

cd "$(dirname "$0")/.."

git diff --name-only "$@" -- . \
    ':(exclude)*.md' ':(exclude)doc/**' ':(exclude)docs/**' ':(exclude)LICENSE'
