# pq tui — v0.14 dev.to cover image (charm.sh/vhs)
#
# Drives `pq tui` through a guided ANALYZE so we can capture the moment
# the new `● pruned: NN% (...)` row appears in the Explain panel.
# The PNG produced by the Screenshot directive becomes the dev.to
# cover_image; the GIF is a fallback for surfaces that animate covers.
#
# Regenerate locally:
#   bash scripts/make_cover_v0.14.sh
#
# Aspect / sizing notes:
#   1280 × 540 ≈ 2.37:1, the dev.to cover ratio (1000×420 = 2.38:1) so
#   nothing important gets cropped. FontSize 13 buys ~99 cols × ~30
#   rows — wide enough for the 4-panel TUI without the Explain panel
#   wrapping. Slightly tighter than tui.tape (which targets demo gifs,
#   not feed thumbnails), intentional.

Output assets/cover-v0.14.gif

Set Shell zsh
Set FontSize 13
Set Width 1280
Set Height 540
Set Theme "GitHub Dark"
Set TypingSpeed 50ms
Set PlaybackSpeed 0.8

# Boot the TUI on the freshly-cooked 5M-row demo parquet.
Type "./target/release/pq tui /tmp/cover_demo.parquet"
Sleep 600ms
Enter
Sleep 1500ms

# Tab into the Query panel and write a filter that pq's predicate
# pushdown can land on the parquet scan AND that DuckDB's row-group
# pruner can act on. `id < 1000000` against a 5M-row file with
# monotonic id = pruning ~80% of row groups (green territory).
Tab
Sleep 300ms
Type ".id, .country, .revenue where .id < 1000000"
Sleep 1500ms

# Open the Explain panel (lowercase e — plain EXPLAIN first, so the
# user sees the "no actuals yet" state before ANALYZE adds them).
Escape
Sleep 250ms
Type "e"
Sleep 1000ms

# ANALYZE (capital E). v0.14 fires a SECOND best-effort EXPLAIN
# ANALYZE under the hood with `enable_profiling='json'` set, pulls
# operator_rows_scanned from the profile, divides by parquet's
# self-reported total rows, renders the gauge.
Type "E"
Sleep 2800ms

# Capture the cover image at peak. The Explain panel is now showing
# all v0.14 data: predicate pushdown, projection pushdown, actual
# rows, AND the new green `● pruned: 80% (...)` line.
Screenshot assets/cover-v0.14.png

# Linger on the result for the GIF, then exit cleanly.
Sleep 1500ms
Escape
Sleep 300ms
Escape
