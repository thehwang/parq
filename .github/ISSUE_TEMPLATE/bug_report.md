---
name: Bug report
about: Something pq does wrong — a crash, a wrong result, garbled output, a perf cliff.
title: "<short symptom> (vX.Y.Z)"
labels: bug
assignees: ""
---

<!--
  Good bug reports for pq pin the DuckDB version and give a reproducer
  that doesn't need your private data. The v0.14.1 pruning bug (#12) is
  the gold standard: it named the exact pragma, the DuckDB version, and
  shipped a Python probe that reproduced the misbehaviour in 10 lines.
-->

## What happened

<!-- The actual behaviour. Paste the wrong output verbatim, including any garbled bytes. -->

## What you expected

## Reproducer

<!--
  Smallest command that triggers it. If it needs a parquet file, prefer
  generating a synthetic one so anyone can run it:

    duckdb -c "COPY (SELECT range AS id FROM range(100)) TO '/tmp/r.parquet'"
    pq /tmp/r.parquet '...'
-->

```bash
pq ...
```

## Environment

- `pq --version`:
- OS / arch:
- DuckDB version (if known — it's in `Cargo.lock` under `libduckdb-sys`):
- Install method: <!-- Homebrew / cargo install / release binary / built from source -->

## Notes

<!-- Stack trace, RUST_BACKTRACE=1 output, suspected root cause, a link to the offending line. -->
