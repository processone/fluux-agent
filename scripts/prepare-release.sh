#!/usr/bin/env bash
#
# Prepare a release: update Cargo.toml version, stamp the CHANGELOG.md date,
# and generate RELEASE_NOTES.md for the GitHub Release action.
#
# Usage:
#   ./scripts/prepare-release.sh 0.1.0
#
# This does NOT create a git tag — you review the changes, commit, then tag.

set -euo pipefail

VERSION="${1:-}"

if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>"
  echo "Example: $0 0.1.0"
  exit 1
fi

# Strip leading 'v' if provided (e.g. v0.1.0 -> 0.1.0)
VERSION="${VERSION#v}"

ROOT="$(git rev-parse --show-toplevel)"
CARGO_TOML="$ROOT/Cargo.toml"
CHANGELOG="$ROOT/CHANGELOG.md"
RELEASE_NOTES="$ROOT/RELEASE_NOTES.md"
TODAY=$(date +%Y-%m-%d)

if [ ! -f "$CHANGELOG" ]; then
  echo "Error: CHANGELOG.md not found at $CHANGELOG"
  exit 1
fi

# 1. Update version in Cargo.toml
echo "Updating Cargo.toml version to $VERSION..."
sed -i.bak -E "s/^version = \"[^\"]+\"/version = \"$VERSION\"/" "$CARGO_TOML"
rm -f "$CARGO_TOML.bak"

# 2. Update Cargo.lock
echo "Updating Cargo.lock..."
(cd "$ROOT" && cargo generate-lockfile 2>/dev/null || cargo check 2>/dev/null || true)

# 3. Stamp version date in CHANGELOG.md
# If "[VERSION] - YYYY-XX-XX" or "[VERSION] - 20XX-XX-XX" exists, replace the date.
# Otherwise, insert a new heading after [Unreleased].
if grep -q "## \[$VERSION\]" "$CHANGELOG"; then
  echo "Stamping date on existing [$VERSION] entry..."
  sed -i.bak -E "s/^## \[$VERSION\] - .*/## [$VERSION] - $TODAY/" "$CHANGELOG"
  rm -f "$CHANGELOG.bak"
else
  echo "Adding [$VERSION] - $TODAY after [Unreleased]..."
  awk -v ver="$VERSION" -v today="$TODAY" '
    /^## \[Unreleased\]/ { print; print ""; print "## [" ver "] - " today; next }
    { print }
  ' "$CHANGELOG" > "$CHANGELOG.tmp"
  mv "$CHANGELOG.tmp" "$CHANGELOG"
fi

# 4. Extract release notes for this version from CHANGELOG.md
# Grab everything between "## [VERSION]" and the next "## [" heading (or EOF).
echo "Generating RELEASE_NOTES.md..."
awk -v ver="$VERSION" '
  BEGIN { found=0 }
  /^## \[/ {
    if (found) exit
    if (index($0, "[" ver "]")) { found=1; next }
  }
  found { print }
' "$CHANGELOG" > "$RELEASE_NOTES.tmp"

# Trim leading/trailing blank lines (portable across macOS and Linux)
BODY=$(awk 'NF {found=1} found' "$RELEASE_NOTES.tmp" | awk '{lines[NR]=$0} END {for(i=NR;i>=1;i--) if(lines[i]!="") {last=i; break} for(i=1;i<=last;i++) print lines[i]}')
rm -f "$RELEASE_NOTES.tmp"

# Read repository URL from Cargo.toml
REPO_URL=$(grep '^repository' "$CARGO_TOML" | sed -E 's/repository = "(.*)"/\1/')

if [ -z "$BODY" ]; then
  echo "Warning: No changelog entries found for version $VERSION."
  BODY="Release v$VERSION"
fi

# Write in fluux release notes format
{
  echo "## What's New in v$VERSION"
  echo ""
  echo "$BODY"
  echo ""
  echo "---"
  echo "[Full Changelog](${REPO_URL}/blob/main/CHANGELOG.md)"
} > "$RELEASE_NOTES"

echo ""
echo "Done! Review the changes:"
echo "  - $CARGO_TOML  (version = \"$VERSION\")"
echo "  - $CHANGELOG   ([$VERSION] - $TODAY)"
echo "  - $RELEASE_NOTES (extracted for GitHub Release)"
echo ""
echo "Next steps:"
echo "  git add Cargo.toml Cargo.lock CHANGELOG.md RELEASE_NOTES.md"
echo "  git commit -m \"Release v$VERSION\""
echo "  git tag v$VERSION"
echo "  git push && git push --tags"
