#!/usr/bin/env bash
set -euo pipefail
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
review_deadline=2027-01-12
today="$(date -u +%F)"
if [[ "$today" == "$review_deadline" || "$today" > "$review_deadline" ]]; then
  echo "Dependency maintenance exceptions require renewed review (deadline $review_deadline)." >&2
  exit 1
fi
cd "$repo_dir/monero-oxide-output"
cargo deny --config ../deny.toml check advisories "$@"
