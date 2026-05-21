#!/usr/bin/env bash
# scripts/demo.sh — drives the asciinema cast for the README GIF.
#
# Run via:   asciinema rec demo.cast --command "./scripts/demo.sh"
# Convert:   agg demo.cast demo.gif --speed 2 --theme github-dark
#
# The cast is intentionally short (~30s) — first impression on HN is everything.

set -e

# Type-it-out helper: prints the prompt + command slowly, then runs the command.
# Slow typing makes the cast feel hand-driven, not robotic.
typeit() {
    local cmd="$*"
    printf '\033[1;32m$\033[0m '
    for ((i = 0; i < ${#cmd}; i++)); do
        printf '%s' "${cmd:$i:1}"
        sleep 0.04
    done
    printf '\n'
    sleep 0.4
    eval "$cmd"
    sleep 1.2
}

clear

printf '\033[1;36m# pq — jq for Parquet. Watch this.\033[0m\n\n'
sleep 1.5

typeit "pq sample.parquet"

typeit 'pq sample.parquet '\''.email where .country == "US"'\'''

typeit "pq schema sample.parquet"

typeit "pq stats sample.parquet"

typeit "pq sample.parquet '.email' | head -3"

printf '\n\033[1;36m# 50 ms cold start. 33 MB single binary. No Python, no JVM.\033[0m\n'
printf '\033[1;36m# github.com/thehwang/parq\033[0m\n'
sleep 2.5
