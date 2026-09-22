#!/bin/sh
# Prints the CHANGELOG.md section for one version, the body of its GitHub release.
# Fails if the section is missing or empty.
set -eu

version="${1:?usage: scripts/release-notes.sh <X.Y.Z>}"
changelog="$(dirname "$0")/../CHANGELOG.md"

notes="$(awk -v heading="## $version - " '
  index($0, heading) == 1 { found = 1; next }
  found && /^## / { exit }
  found { print }
' "$changelog" | sed -e '/./,$!d')"

if [ -z "$notes" ]; then
  echo "release-notes: no CHANGELOG.md section \"## $version - <date>\"" >&2
  exit 1
fi
printf '%s\n' "$notes"
