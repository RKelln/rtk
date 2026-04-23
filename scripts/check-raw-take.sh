#!/usr/bin/env bash
# check-raw-take.sh — guard against unguarded lossy .take(N) caps
#
# Scans src/cmds/ for .take(<literal-integer>) calls that are NOT using
# config::lossless_cap(). Every new lossy output cap must go through
# lossless_cap() so --lossless / lossless = true is respected.
#
# To silence a legitimate exception, add one of these trailing comments:
#   // summarization  — intentional top-N stat (e.g. "Top linters", "Top files")
#   // display        — chars().take(N) line-width truncation
#   // internal       — intermediate pipeline data structure, not user-visible output
#
# Usage:
#   bash scripts/check-raw-take.sh          # exits 1 if violations found
#   bash scripts/check-raw-take.sh --warn   # prints but exits 0 (CI soft-mode)

set -euo pipefail

WARN_ONLY=0
if [[ "${1:-}" == "--warn" ]]; then
  WARN_ONLY=1
fi

# Find all .take(<integer>) calls — these are the candidates
bad=$(grep -rn '\.\(take\)([0-9]' src/cmds/ \
  | grep -v 'chars()\.take('         \
  | grep -v '// summarization'       \
  | grep -v '// display'             \
  | grep -v '// internal'            \
  | grep -v 'lossless_cap(' || true)

if [ -z "$bad" ]; then
  echo "✓ check-raw-take: no unguarded lossy .take(N) found"
  exit 0
fi

echo ""
echo "ERROR: Unguarded lossy .take(N) found in src/cmds/"
echo ""
echo "All output item caps must use config::lossless_cap(N) so --lossless is respected."
echo "Replace:"
echo "  items.iter().take(10)"
echo "With:"
echo "  items.iter().take(config::lossless_cap(10))"
echo ""
echo "If this is intentional (top-N stat / display-only / internal pipeline),"
echo "add a trailing comment: // summarization  OR  // display  OR  // internal"
echo ""
echo "Violations:"
echo "$bad"
echo ""

if [ "$WARN_ONLY" -eq 1 ]; then
  exit 0
fi
exit 1
