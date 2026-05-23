# pq tui — VHS tape (charm.sh/vhs)
#
# Drives the v0.5+ interactive TUI through a guided walkthrough so we
# can:
#   1. Regenerate assets/tui.gif on demand (no manual asciinema dance)
#   2. Smoke-test the TUI in CI on every PR — VHS exits non-zero if
#      pq panics or the screen blanks, catching regressions that the
#      snapshot tests can't (e.g. a panic during compile, terminal
#      teardown bugs, focus/redraw glitches).
#
# Regenerate locally:
#   brew install vhs              # macOS
#   cargo build --release         # so the demo uses the latest binary
#   vhs assets/tui.tape           # writes assets/tui.gif
#
# In CI we run with --quiet against a known-good sample.parquet and
# discard the gif — the only signal we care about is exit code.

Output assets/tui.gif

Set Shell zsh
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "GitHub Dark"
Set TypingSpeed 70ms
Set PlaybackSpeed 0.7

# Render the prompt for one frame so the user sees what we're about to launch.
Type "./target/release/pq tui sample.parquet"
Sleep 800ms
Enter
Sleep 1500ms

# Tab into Query, type a real DSL — the highlight in Columns ★'s the
# referenced fields live, which is the v0.6 "wow" moment.
Tab
Sleep 400ms
Type ".country, .revenue where .country == "
Type `"`
Type "US"
Type `"`
Sleep 1500ms

# Group-by + sum-aggregate to show the rewrite engine picking up
# `group_by` / `sum` and emitting the right SQL behind the scenes.
Ctrl+a
Backspace
Type "group_by .country | sum .revenue | top 3 by sum_revenue"
Sleep 2000ms

# Show off the Explain panel (predicate / projection pushdown facts +
# heuristic 💡 hints). v0.6 feature.
Escape
Sleep 300ms
Type "e"
Sleep 1800ms

# Run EXPLAIN ANALYZE — async worker, status header ticks "running…",
# then swaps to "ANALYZE 1.2 ms" with actual rows. v0.7 feature.
Type "E"
Sleep 1800ms

# Help overlay shows every keybinding, including v0.8's Ctrl-↑/↓
# history navigation.
Type "?"
Sleep 2200ms

# Dismiss + clean exit so VHS reports success.
Space
Sleep 300ms
Escape
Sleep 500ms
