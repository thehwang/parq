#!/usr/bin/env bash
# scripts/demo.sh — drives the asciinema cast for the README GIF.
#
# Run via:   asciinema rec demo.cast --command "./scripts/demo.sh"
# Convert:   agg demo.cast demo.gif --speed 2 --theme github-dark
#
# The cast is intentionally short (~30s) — first impression on HN is everything.

# Don't `set -e` — asciinema's pty doesn't always have a real TTY, and we want
# the cast to continue even if a single demo command fails.

# Use local release binary unless one is already on PATH.
PQ="${PQ:-./target/release/pq}"

# Type-it-out helper: prints the prompt + command slowly, then runs the command.
# Slow typing makes the cast feel hand-driven, not robotic. We display "pq" but
# actually execute $PQ (which usually expands to ./target/release/pq).
typeit() {
    local display="$1"
    local exec_cmd="${2:-$1}"
    printf '\033[1;32m$\033[0m '
    for ((i = 0; i < ${#display}; i++)); do
        printf '%s' "${display:$i:1}"
        sleep 0.04
    done
    printf '\n'
    sleep 0.4
    eval "$exec_cmd"
    sleep 1.2
}

printf '\033[H\033[2J'  # clear screen via ANSI (works even without a real TTY)

printf '\033[1;36m# pq -- jq for Parquet. Watch this.\033[0m\n\n'
sleep 1.5

# Force -o table on the displayed-as-pretty commands.
# Pipe demos intentionally rely on auto → ndjson when piped.
typeit "pq sample.parquet" \
       "$PQ -o table sample.parquet"

typeit "pq sample.parquet '.email where .country == \"US\"'" \
       "$PQ -o table sample.parquet '.email where .country == \"US\"'"

typeit "pq sample.parquet 'group_by .country | sum .revenue | top 3 by sum_revenue'" \
       "$PQ -o table sample.parquet 'group_by .country | sum .revenue | top 3 by sum_revenue'"

typeit "pq schema sample.parquet" \
       "$PQ -o table schema sample.parquet"

typeit "pq sample.parquet 'where .country == \"US\"' -o parquet > /tmp/us.parquet && pq /tmp/us.parquet" \
       "$PQ sample.parquet 'where .country == \"US\"' -o parquet > /tmp/us.parquet && $PQ -o table /tmp/us.parquet"

typeit "pq sample.parquet '.email' | head -3" \
       "$PQ sample.parquet '.email' | head -3"

printf '\n\033[1;36m# ~50 ms cold start. 33 MB single binary. No Python, no JVM.\033[0m\n'
printf '\033[1;36m# github.com/thehwang/parq\033[0m\n'
sleep 2.5
