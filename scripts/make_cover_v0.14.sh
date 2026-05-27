#!/usr/bin/env bash
# Generate the dev.to cover image for the v0.14 Finish-Up-A-Thon submission.
#
# Output: assets/cover-v0.14.png   (single-frame, 1280x540, dev.to-friendly)
#         assets/cover-v0.14.gif   (animated, ~10s, fallback for richer feeds)
#
# What it shows: the new TUI Explain panel post-ANALYZE, with the v0.14
# `● pruned: 80% (...)` row highlighted in green. Drives a real `pq tui`
# session against a freshly-cooked 5M-row parquet so the numbers are
# real (DuckDB actually prunes ~80% of row groups for the demo filter).
#
# Requirements:
#   - vhs           (brew install vhs   /  apt install vhs)
#   - python3 + pandas + pyarrow  (pip install pandas pyarrow)
#   - cargo build --release   (so the demo runs the v0.14.0 binary)
#
# Usage:
#   bash scripts/make_cover_v0.14.sh

set -euo pipefail

cd "$(dirname "$0")/.."

DEMO_PARQUET=/tmp/cover_demo.parquet
PQ_BIN=./target/release/pq

# 1. Always rebuild — the cover image MUST show the v0.14 pruning
#    ratio row, which only exists in 0.14.0+. Cargo is incremental so
#    a no-op rebuild is sub-second; better than silently re-using a
#    stale 0.12 / 0.13 binary if one happens to be on disk (which
#    produces a comically-broken cover with JSON debris in the panel).
echo "[cover] building release binary…"
cargo build --release
echo "[cover] $($PQ_BIN --version)"

# 2. Cook the demo parquet if it doesn't exist (5M rows takes ~3 s).
#    Skipping the regen on subsequent runs makes iteration on the tape
#    fast — `vhs assets/cover-v0.14.tape` alone is the inner loop.
if [[ ! -f "$DEMO_PARQUET" ]]; then
  echo "[cover] cooking $DEMO_PARQUET (5M rows)…"
  python3 scripts/make_cover_demo.py "$DEMO_PARQUET"
fi

# 3. Run the tape. VHS writes both the GIF (full playback) and the PNG
#    (single-frame Screenshot directive captured at the post-ANALYZE
#    moment). The PNG is what we feed to dev.to as `cover_image`.
if ! command -v vhs >/dev/null 2>&1; then
  echo "[cover] vhs not on PATH — install with: brew install vhs"
  exit 1
fi

echo "[cover] running vhs assets/cover-v0.14.tape…"
vhs assets/cover-v0.14.tape

echo
echo "[cover] done."
echo "  PNG: $(pwd)/assets/cover-v0.14.png  (use this as cover_image)"
echo "  GIF: $(pwd)/assets/cover-v0.14.gif  (use as inline demo if you like)"
