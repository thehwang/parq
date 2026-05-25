# `pq` — Reference Manual

> `pq` is a Rust single-binary CLI that embeds DuckDB to query Parquet files
> with a jq-style DSL. This document is the **lookup-style reference
> manual**: feature-by-feature, non-linear. Comprehensive through v0.11;
> v0.12 (streaming output, Ctrl-C interrupt, `count --lite`) and v0.13
> (async TUI preview, stderr spinner, `stats --lite`) are documented in
> the project [README.md](../README.md) and [tutorial.md §Lesson 5](./tutorial.md#lesson-5-big-file-mode-5-min--v012--v013)
> until this manual catches up.

> **First time with pq?** Read [`tutorial.md`](./tutorial.md) instead —
> a 30-minute hands-on walkthrough that takes you from "what is pq" to
> "writing real queries against my own data". Much friendlier than this manual.
>
> **Already know pq, here to look something up?** You're in the right place —
> the table of contents is below.

> **Two ways to use it — pick whichever fits**:
> - **Command-line DSL**: `pq f.parquet '.email where .country == "US"'` — jq-style, pipeable, scriptable (see [§4](#4-dsl-pq-expression--duckdb-sql))
> - **Interactive TUI**: `pq tui f.parquet` — lazygit-inspired five-pane UI with semantic sync, schema completion, EXPLAIN ANALYZE on demand (see [§8](#8-interactive-tuipq-tui-file))
>
> Want a 5-minute sampler? See [§3 Quick Examples](#3-quick-examples-5-minute-tour) below. For a full walkthrough, [`tutorial.md`](./tutorial.md) is the better entry.

---

## Table of Contents

- [1. Project Positioning](#1-project-positioning)
- [2. Top-level CLI](#2-top-level-clipq---help)
  - [2.1 Invocation Forms](#21-invocation-forms)
  - [2.2 Global Flags](#22-global-flags)
  - [2.3 Subcommands (no DSL needed)](#23-subcommands-no-dsl-needed)
- [**3. Quick Examples (5-minute tour)**](#3-quick-examples-5-minute-tour)
  - [3.1 Basic filter + projection](#31-basic-filter--projection)
  - [3.2 Group-by aggregate + Top N](#32-group-by-aggregate--top-n)
  - [3.3 Nested data (chained UNNEST)](#33-nested-data-chained-unnest)
  - [3.4 Chaining with jq](#34-chaining-with-jq)
  - [3.5 TUI starter](#35-tui-starter)
- [4. DSL Syntax](#4-dsl-pq-expression--duckdb-sql)
  - [4.1 Grammar overview](#41-grammar-overview)
  - [4.2 v0 single-stage forms](#42-v0-single-stage-forms-still-supported)
  - [4.3 v0.2 pipeline stages](#43-v02-pipeline-stages-piped-with-)
  - [4.4 v0.3 join](#44-v03-join)
  - [4.5 v0.4 line-output sugar](#45-v04-line-output-sugar)
  - [4.6 v0.4 SQL macros (scalar UDF)](#46-v04-sql-macros-scalar-udf)
  - [4.7 Filter expression sugar](#47-filter-expression-sugar)
  - [4.8 v0.10 nested schema path syntax](#48-v010-nested-schema-path-syntax)
  - [4.9 v0.11 chained UNNEST](#49-v011-chained-unnest-eventskind-in-any-clause)
  - [4.10 Raw SQL escape hatch](#410-raw-sql-escape-hatch)
- [5. Data source resolution](#5-data-source-resolution)
  - [5.1 stdin auto-spool (v0.9)](#51-stdin-auto-spool-v09--pq-as-a-shell-primitive)
  - [5.2 Chain idioms](#52-chain-idioms-v09)
- [6. Cloud credentials auto-injection](#6-cloud-credentials-auto-injection)
- [7. Output formats](#7-output-formats)
- [**8. Interactive TUI**](#8-interactive-tuipq-tui-file)
  - [8.1 v0.5 base panels](#81-v05-base-panels)
  - [8.2 v0.6 semantic sync + completion + drill-down + Explain](#82-v06-semantic-sync--schema-completion--drill-down--explain-pane)
  - [8.3 v0.7 EXPLAIN ANALYZE on demand](#83-v07-explain-analyze-on-demand)
  - [8.4 v0.8 async ANALYZE + query history](#84-v08-async-analyze--query-history)
  - [8.5 Full TUI keymap](#85-full-tui-keymap)
- [9. Testing & CI](#9-testing--ci)
- [10. Installation](#10-installation)
- [11. Version history](#11-version-history)
- [12. Roadmap](#12-roadmap)

---

## 1. Project Positioning

| Aspect       | Detail |
|--------------|--------|
| Language/runtime | Rust 2021, single-file binary (~33 MB; ~10 MB after strip) |
| Engine       | DuckDB (`duckdb-rs` 1.1, `bundled` feature, statically linked C++) |
| Cold start   | ~50 ms (no JVM, no Python) |
| Platforms    | macOS arm64 / x86_64, Linux x86_64 musl, Windows x86_64 MSVC |
| Distribution | GitHub Release prebuilt binaries, `install.sh` one-liner, Homebrew tap, `cargo install pq` |
| License      | MIT |

Design goal: collapse the duckdb CLI's verbose
`SELECT email FROM 'file.parquet' WHERE country='US'` down to
`.email where .country == "US"` — without giving up any of DuckDB's power.

---

## 2. Top-level CLI (`pq --help`)

### 2.1 Invocation forms

```
pq <FILE>                         # default: head 20
pq <FILE> '<QUERY>'               # run DSL
pq <SUBCOMMAND> <FILE>            # built-in subcommands
pq tui <FILE>                     # enter the TUI
```

### 2.2 Global flags

| flag                  | description |
|-----------------------|-------------|
| `-o, --output`        | `auto` / `table` / `json` / `ndjson` / `csv` / `parquet`. `auto` = table on TTY, ndjson on pipe |
| `-i, --input` (v0.9)  | `auto` / `parquet` / `ndjson` (`jsonl`/`json`) / `csv` (`tsv`). `auto` sniffs by file extension; stdin (`-`) defaults to parquet |
| `-n, --n N`           | default head row count, default 20. `-n 0` = unlimited. Auto-set to 0 with `-o parquet` |
| `--explain`           | print the SQL pq compiled, don't execute |
| `-w, --watch SECS`    | re-run every N seconds, like `watch -n` |
| `--udf 'f(x):=...'`   | register a DuckDB SQL macro; repeatable |

### 2.3 Subcommands (no DSL needed)

| Subcommand               | Behavior                                                |
|--------------------------|---------------------------------------------------------|
| `pq schema FILE`         | column names / types / nullability                      |
| `pq stats FILE`          | per-column min / max / null% / approx distinct          |
| `pq count FILE`          | total row count                                         |
| `pq head FILE -n N`      | first N rows                                            |
| `pq tail FILE -n N`      | last N rows (uses reverse `row_number()` window)        |
| `pq sample FILE -n N`    | random N rows (`USING SAMPLE`)                          |
| `pq tui FILE`            | enter the interactive TUI                               |

---

## 3. Quick examples (5-minute tour)

> Assume you have an `events.parquet` with this schema:
> `user_id INTEGER, country VARCHAR, revenue DOUBLE, ts TIMESTAMP,`
> `events LIST<STRUCT(kind VARCHAR, amount DOUBLE)>`.
> Don't have one? Runnable samples live in the repo's `examples/` folder.

### 3.1 Basic filter + projection

```bash
# Peek at the schema
pq schema events.parquet

# Project + filter (v0 inline form, shortest)
pq events.parquet '.user_id, .country, .revenue where .country == "US"'

# Equivalent staged form — pipe-chained, closer to jq / SQL mental model
pq events.parquet 'where .country == "US" | .user_id, .country, .revenue'
# {"user_id":1,"country":"US","revenue":12.34}
# {"user_id":2,"country":"US","revenue": 4.56}
```

### 3.2 Group-by aggregate + Top N

```bash
# Total revenue per country, top 5
pq events.parquet 'group_by .country | sum .revenue | top 5 by sum_revenue'
# {"country":"US","sum_revenue":12340.5}
# {"country":"UK","sum_revenue":4567.0}

# Multi-key group + multiple aggregates
pq events.parquet 'group_by .country | count, avg .revenue | sort by .country'
# {"country":"UK","count":42,"avg_revenue":108.74}
# {"country":"US","count":113,"avg_revenue":109.21}
```

### 3.3 Nested data (chained UNNEST, v0.11)

```bash
# events is LIST<STRUCT> — group by event type in a single DSL line
pq events.parquet 'group_by .events[].kind | count | sort by .count desc'
# {"events_kind":"click","count":2400}
# {"events_kind":"buy","count":312}

# Refer to the same exploded list across columns — pq dedupes the UNNEST
# (you get N rows, not N*N)
pq events.parquet '.user_id, .events[].kind, .events[].amount | head 5'

# Want to see the SQL pq generated? Add --explain
pq events.parquet 'group_by .events[].kind | count' --explain
# SELECT _pq_u0.kind AS events_kind, count(*) AS count
# FROM (SELECT *, UNNEST(events) AS _pq_u0 FROM read_parquet('events.parquet')) AS _pq_src
# GROUP BY _pq_u0.kind
```

### 3.4 Chaining with jq

```bash
# pq emits ndjson → jq filters → pq aggregates again
pq events.parquet '.user_id, .ts | to_json' \
  | jq -c 'select(.ts > "2026-01-01")' \
  | pq -i ndjson - 'count'
# {"count":847}

# Pipe parquet directly — pq drains a non-seekable stdin to a tempfile (see §5.1)
cat sales.parquet | pq - 'group_by .country | sum .revenue'
```

### 3.5 TUI starter

```bash
pq tui events.parquet
```

Once inside:
- `Tab` cycles between Columns / Query / Data; `Space` toggles a column into the projection
- The Query pane is a textarea — type DSL and **every keystroke re-runs the query**, the Data pane reflects the first 50 rows live
- Move the Data cursor to a row and press `Enter` → pq auto-builds a `where` clause from that row's group-by values (drill-down)
- `e` opens the Explain pane (estimates + heuristic hints), `E` runs EXPLAIN ANALYZE async
- `:` shows the real compiled SQL, `?` shows the keymap overlay
- `Q` quits and prints the equivalent `pq <file> '<query>'` to stdout — copy/paste that into a script

Full keymap in [§8.5](#85-full-tui-keymap), pane-by-pane interaction in [§8](#8-interactive-tuipq-tui-file).

---

## 4. DSL (pq expression → DuckDB SQL)

### 4.1 Grammar overview

```
query     := stage ( '|' stage )*
           | <raw SELECT/WITH/EXPLAIN/PRAGMA>      (forwarded, not rewritten)
           | <empty>                               (= SELECT * LIMIT n)
```

### 4.2 v0 single-stage forms (still supported)

| Syntax                                  | Meaning |
|-----------------------------------------|---------|
| `.col`                                  | project a column |
| `.user.id`                              | nested struct path |
| `.email, .name, .country`               | project several columns |
| `country == "US"`                       | bare-expression filter |
| `.email where .country == "US"`         | inline projection + filter (v0 shorthand) |

### 4.3 v0.2 pipeline stages (piped with `|`)

| Stage                                       | Description |
|---------------------------------------------|-------------|
| `select .col1, .col2`                       | explicit projection (`select` prefix optional) |
| `where <expr>`                              | filter — routes to WHERE if no grouping yet, HAVING after a grouping verb |
| `group_by .col1, .col2`                     | grouping (alias: `group by`) |
| `count`                                     | `count(*) AS count` |
| `sum .col` / `avg .col` / `min .col` / `max .col` / `count_distinct .col` | aggregates, auto-aliased `sum_col` etc. |
| `top N by <col>`                            | `ORDER BY <col> DESC LIMIT N` |
| `sort by <col> [asc\|desc]`                 | sort (alias: `order by`) |
| `limit N` / `head N` / `head -n N` / `head -N` | row cap; v0.11 also accepts the unix `head` flag forms |
| `distinct`                                  | adds `DISTINCT` |

### 4.4 v0.3 join

```
pq a.parquet 'join "b.parquet" on .a.id == .b.user_id | group_by .a.country | sum .b.amount'
```

| Form                                          | Meaning |
|-----------------------------------------------|---------|
| `join "b.parquet" on .col`                    | INNER, equi `a.col = b.col` shorthand |
| `join "b.parquet" on .a.id == .b.user_id`     | INNER, both sides explicit |
| `left_join "b" on ...`                        | LEFT OUTER JOIN |
| `right_join "b" on ...`                       | RIGHT OUTER JOIN |
| `full_join "b" on ...`                        | FULL OUTER JOIN |
| `on .a.x == .b.x and .a.y == .b.y`            | multi-key join (just compose with `and`/`AND`) |

Subsequent stages refer to the two sides as `.a.col` / `.b.col`.

### 4.5 v0.4 line-output sugar

| Stage      | Equivalent SQL                                              | Use case |
|------------|-------------------------------------------------------------|----------|
| `to_csv`   | `concat_ws(',', col1::TEXT, col2::TEXT, ...)`               | barest CSV (no quoting), one row per line |
| `to_json`  | `to_json({col1: col1, col2: col2, ...})`                    | DuckDB struct → JSON |
| `to_ndjson` / `to_jsonl` | aliases for `to_json` (v0.9.1)                | unix-friendly names |

The output renderer auto-switches to raw-lines mode and overrides `-o`. Typical use: `pq f.parquet '.email | to_json' | jq ...`.

### 4.6 v0.4 SQL macros (scalar UDF)

```
pq f.parquet --udf 'is_us(c) := c = ''US''' '.email where is_us(.country)'
```

`:=` is rewritten by pq into DuckDB's `CREATE OR REPLACE MACRO ... AS ...`.
`--udf` is repeatable. Also accepts user-written `name(args) AS body` or a complete `CREATE MACRO`.

### 4.7 Filter expression sugar

| pq syntax         | compiled SQL          |
|-------------------|-----------------------|
| `"foo"`           | `'foo'`               |
| `==`              | `=`                   |
| `!=`              | `<>`                  |
| `.col`            | `col`                 |
| anything else     | passed through to DuckDB |

### 4.8 v0.10 nested schema path syntax

jq-style `[]` / `[N]` / `["k"]` are wired into pq's path tokenizer. The
three Parquet nested types — `STRUCT` / `LIST` / `MAP` — are first-class:

| pq DSL                       | DuckDB SQL                                | Use |
|------------------------------|-------------------------------------------|-----|
| `.user.name`                 | `user.name`                               | STRUCT field (since v0) |
| `.tags[0]`                   | `tags[1]` (jq 0-idx → DuckDB 1-idx)       | LIST index |
| `.tags[-1]`                  | `tags[-1]` (DuckDB native negative idx)   | last element |
| `.tags[]`                    | `UNNEST(tags) AS tags`                    | row explosion (projection only at v0.10) |
| `.events[0].kind`            | `events[1].kind`                          | LIST&lt;STRUCT&gt; index-then-field |
| `.metadata["plan"]`          | `element_at(metadata, 'plan')[1]`         | MAP value lookup (double quotes) |
| `.metadata['plan']`          | same                                      | single quotes |
| `len(.tags)` / `length(.tags)` | `len(tags) AS len_tags`                | length |
| `keys(.metadata)`            | `map_keys(metadata) AS keys_metadata`     | MAP keys |
| `values(.metadata)`          | `map_values(metadata) AS values_metadata` | MAP values |

#### Auto-alias

Bracket-bearing paths get a snake_case alias so JSON keys don't bleed
DuckDB internals (`(events[1]).amount` / `element_at(metadata, 'plan')[1]`):

| Path                | JSON key           |
|---------------------|--------------------|
| `.tags[0]`          | `tags_0`           |
| `.tags[-1]`         | `tags_neg1`        |
| `.events[0].amount` | `events_0_amount`  |
| `.metadata["plan"]` | `metadata_plan`    |
| `.tags[]`           | `tags` (UNNEST goes back to flat) |
| `len(.tags)`        | `len_tags`         |

Pure struct dot-paths (`.user.email`) keep DuckDB's default naming
(`email`) — backward-compat contract from v0.9.x.

#### Renderer upgrades

LIST / STRUCT / MAP no longer print as Rust `Debug`; they render as
proper JSON:

```bash
# Before (v0.9.x)
{"events":"List([Struct(OrderedMap([(\"kind\", Text(\"click\")), ..."}
# Now (v0.10)
{"events":[{"kind":"click","amount":1.0},{"kind":"buy","amount":9.0}]}
```

JSON output also **preserves SELECT order** (via `serde_json`'s
`preserve_order` feature), so positional jq filters across versions
stay stable.

#### Limitations (v0.10)

* List-comprehension style (`.events[? .amount > 5]`) still routes to the
  raw SQL escape hatch (`list_filter(events, e -> e.amount > 5)`).
* In v0.10, `[]` is only legal in projections — `where .tags[]` etc. is
  rejected. v0.11 lifts this: `[]` can be followed by `.field` (chained
  UNNEST), and WHERE / GROUP BY / HAVING / ORDER BY all accept it — see §4.9.

### 4.9 v0.11 chained UNNEST (`.events[].kind` in any clause)

v0.10 marked `.events[].kind` (UNNEST then field access) as the v0.11
roadmap signpost. v0.11 ships it — and not just for projection: it works
in **WHERE / GROUP BY / HAVING / ORDER BY** too.

The classic motivating query:

```bash
pq f.parquet 'group_by .events[].kind | count | sort by .count desc'
# v0.10: Binder Error: UNNEST not supported here
# v0.11: just works
# {"events_kind":"click","count":2}
# {"events_kind":"buy","count":1}
```

#### Implementation: UNNEST hoister (`to_sql_core`)

DuckDB allows bare `UNNEST(events)` only in a top-level SELECT with no
GROUP BY / WHERE / HAVING / ORDER BY at the same level — otherwise it
throws *Binder Error: UNNEST not supported here*. v0.11 detects every
chained `UNNEST(...)` (one followed by `.` / `[`, or sitting in a clause
DuckDB rejects it in) at compile time and **lifts** it into a derived
FROM:

```sql
-- DSL: group_by .events[].kind | count | sort by .count desc
SELECT _pq_u0.kind AS events_kind, count(*) AS count
FROM (SELECT *, UNNEST(events) AS _pq_u0
      FROM read_parquet('f.parquet')) AS _pq_src
GROUP BY _pq_u0.kind ORDER BY count DESC
```

The outer SELECT/WHERE/GROUP BY only see plain column refs to
`_pq_u0.kind`, so DuckDB has nothing to complain about. Cost: one extra
SELECT layer per query — DuckDB's optimizer flattens it; wall-clock is
effectively unchanged.

#### Shared-source dedup

Multiple references to the same exploded LIST share one UNNEST — no
cartesian explosion:

```bash
pq f.parquet '.events[].kind, .events[].amount'
# Inner FROM has exactly one UNNEST(events) AS _pq_u0
# Output row count = len(events), not len(events)^2
```

Dedup key is the verbatim inner expression of `UNNEST(...)`, so
`.payer_zipped[].type_coverage` and `.payer_zipped[].payer_id` share
`_pq_u0` too.

#### Works in every clause (real-world)

```bash
# Projection
pq f.parquet '.user_id, .events[].kind, .events[].amount'

# Aggregation
pq f.parquet 'sum .events[].amount'             # → {"sum_events_amount": 11.0}
pq f.parquet 'group_by .events[].kind | sum .events[].amount'

# Filter
pq f.parquet 'where .events[].kind == "click" | count'

# Sort
pq f.parquet '.user_id, .events[].amount | sort by .events[].amount desc | head 5'

# Multiple sources coexisting (payer + provider)
pq f.parquet 'group_by .payer_zipped[].type_coverage,
                       .provider_zipped[].provider_specialization_marked
              | count | top 10 by count'
```

#### Alias cleanup

In v0.10, `sum .events[].amount` produced the eyesore alias
`sum_UNNEST_events__amount` (because `alias_safe` mapped every
non-identifier byte to `_`). v0.11 strips `UNNEST(<inner>)` wrappers
*before* sanitizing, so it now reads `sum_events_amount` — consistent
with v0.10's path aliases like `events_0_amount`.

#### See the real SQL with `--explain`

```bash
pq f.parquet 'group_by .events[].kind | count' --explain
# SELECT _pq_u0.kind AS events_kind, count(*) AS count
# FROM (SELECT *, UNNEST(events) AS _pq_u0 FROM read_parquet('f.parquet')) AS _pq_src
# GROUP BY _pq_u0.kind
```

#### Limitations (v0.11)

* When one side of a JOIN contains a chained UNNEST, the hoister still
  wraps the entire `<source> AS a JOIN <right> AS b ON ...` as a derived
  table. Works in practice, but aliases `a` / `b` pass through to the
  outer query unchanged — for complex mixed shapes, raw SQL is still the
  pragmatic option.
* "Filter list elements but keep the original row" (jq's `.[? .kind ==
  "click"]`) still requires `list_filter(...)` raw SQL — `[]` row
  explosion semantics are inherently incompatible with row preservation.

### 4.10 Raw SQL escape hatch

If a query starts with `SELECT ` / `WITH ` / `EXPLAIN ` / `PRAGMA `, pq
doesn't rewrite it — it just substitutes the literal `FILE` placeholder
with `read_parquet('...')` and forwards the rest to DuckDB. Lets you
reach for window functions, CTEs, `UNPIVOT` and the rest of DuckDB's
toolbox when you need to.

---

## 5. Data source resolution

`source_clause_fmt()` (since v0.9) picks the right DuckDB `read_*` table
function based on `--input` and handles these cases:

| Input form                              | Behavior |
|-----------------------------------------|----------|
| `foo.parquet`                           | local file, `read_parquet('foo.parquet')` |
| `./data/x.parquet`                      | relative path |
| `'data/dt=2026-*/*.parquet'`            | glob — DuckDB expands natively |
| `gs://bucket/path`                      | via httpfs extension, credentials auto-injected (see §6) |
| `s3://bucket/path`                      | same |
| `az://...`, `http(s)://...`             | same |
| `-`                                     | stdin (see §5.1) |
| `path/dt=YYYY-MM-DD/region=X/*.parquet` | auto-detected hive partitioning, adds `hive_partitioning=true`, partition columns become regular queryable columns (parquet only) |
| `data.ndjson` / `data.jsonl`            | (v0.9) `--input auto` routes by extension to `read_json(format='newline_delimited', auto_detect=true)` |
| `data.csv` / `data.tsv`                 | (v0.9) `--input auto` routes by extension to `read_csv_auto(...)` |

### 5.1 stdin auto-spool (v0.9) — pq as a shell primitive

DuckDB's parquet reader needs `lseek` to reach the footer; the
line-oriented `read_json` / `read_csv_auto` do schema inference + decode
in two passes — both are fundamentally incompatible with non-seekable
anonymous pipes. v0.9 added stdin auto-spool to bridge the gap:

| Command form | What pq does |
|--------------|--------------|
| `pq - < f.parquet` | fd is a regular file (`<` redirect), seekable — pass `/dev/stdin` straight to DuckDB |
| `cat f.parquet \| pq -` | `lseek(0, 0, SEEK_CUR)` returns ESPIPE, drain stdin into a `tempfile::NamedTempFile`, substitute path `/tmp/pq-stdin-*.parquet` |
| `aws s3 cp s3://x/y - \| pq -` | same |
| `pq f.parquet '...' \| pq -` | same (chain idiom) |
| `pq -i ndjson -` | always spool (read_json is two-pass even on seekable input), suffix `*.ndjson` |
| `pq -i csv -` | same, suffix `*.csv` |

The spool tempfile is owned by `StdinSpool`, dropped via RAII on `main()`
exit — no manual cleanup. On macOS `$TMPDIR` is RAM-backed, so the
"read once → write once" cost is usually absorbed by the page cache.

### 5.2 Chain idioms (v0.9)

```bash
# Parquet straight through
cat sales.parquet | pq - 'group_by .country | sum .revenue'

# Self-describing ndjson chain (recommended)
pq sales.parquet '.country, .revenue | to_json' \
  | pq -i ndjson - 'group_by .country | sum .revenue | top 5 by sum_revenue'

# CSV chain (use -o csv on the producer to keep the header)
pq -o csv sales.parquet '.email, .country' \
  | pq -i csv - '.country | distinct'

# Interop with jq
pq events.parquet '.user_id, .ts | to_ndjson' \
  | jq -c 'select(.ts > "2026-01-01")' \
  | pq -i ndjson - 'count'
```

---

## 6. Cloud credentials auto-injection

When pq opens its DuckDB connection it inspects environment variables and
issues `CREATE OR REPLACE SECRET`:

| Env vars                                     | Secret created |
|----------------------------------------------|----------------|
| `PQ_GCS_HMAC_KEY` + `PQ_GCS_HMAC_SECRET`     | `TYPE GCS, KEY_ID + SECRET` (GCS's S3-compat HMAC, the recommended path) |
| `PQ_GCS_BEARER_TOKEN`                        | `TYPE GCS, BEARER_TOKEN` (best-effort; DuckDB <1.2 may reject — pq silently ignores) |
| `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` | `TYPE S3, KEY_ID + SECRET`, with optional `AWS_SESSION_TOKEN`, `AWS_REGION` / `AWS_DEFAULT_REGION`, `AWS_ENDPOINT_URL_S3` |
| none of the above set                        | `TYPE S3, PROVIDER credential_chain` — auto-walks `AWS_PROFILE`, `~/.aws/credentials`, SSO, EC2 IMDS, ECS task role (v0.5.1) |

Any secret-creation failure is silently swallowed by default; set
`PQ_DEBUG=1` to surface them on stderr. Principle: a stale env var must
never block local-file usage.

---

## 7. Output formats

| `-o` value  | Renderer                | Notes |
|-------------|-------------------------|-------|
| `auto`      | TTY → table, pipe → ndjson | default |
| `table`     | comfy-table, UTF-8 borders, footer `(N rows)` |  |
| `json`      | a single JSON array     |  |
| `ndjson`    | one JSON object per line | pipe-friendly |
| `csv`       | standard CSV with header | comfy-table handles comma escaping |
| `parquet`   | direct `COPY (sql) TO 'stdout' (FORMAT PARQUET)`, binary stream to stdout | auto-sets `-n` to 0 to avoid accidental truncation |
| `raw-lines` | triggered by `to_csv` / `to_json` stages; one printed line per row, no header / quoting | not selectable via `-o` |

---

## 8. Interactive TUI (`pq tui FILE`)

> Inspired by lazygit. **Five panes — Columns / Query / Filters / Data / Explain — plus a status bar.**

### 8.1 v0.5 base panels

| Pane    | Content |
|---------|---------|
| Columns | file schema: name + type; ★ = projected, ▶ = focused |
| Filters | extracted `where` / `having` from the current query (read-only display) |
| Query   | editable textarea (built on tui-textarea), ghost-text template hints |
| Data    | live query, up to `PREVIEW_LIMIT=50` rows, auto-fit column widths, numerics right-aligned |

On exit the TUI prints the equivalent `pq <file> '<query>'` to stdout so
you can paste it straight into a script.

### 8.2 v0.6 semantic sync + schema completion + drill-down + Explain pane

- **Lineage (semantic sync)**: a hand-written tolerant token scanner
  (`src/lineage.rs`) parses possibly-incomplete queries and extracts
  column refs + derived aggregate columns (`sum(.revenue)` →
  `sum_revenue` sourced from `revenue`). Cursor on `.sum_revenue` →
  Columns pane highlights ★ `revenue`; same name in the Query pane gets
  search-highlight color.
- **Schema completion**: typing `.co` in the Query pane pops a completion
  popup, prefix-match first then substring; ↑/↓ select, ⏎/Tab insert.
- **Drill-down**: in the Data pane, place the cursor on a row and press
  ⏎ — pq appends `where .col == val [AND ...]` built from that row's
  group-by values; ⌫ undoes.
- **Explain pane (`e` toggles)**: parses DuckDB's `EXPLAIN <sql>` output
  into structured facts — scan count / estimated rows / pushed filters
  / projection pushdown / file count — and surfaces heuristic hints
  (💡 add `where .partition_date >= ...`, 💡 select fewer columns…).

### 8.3 v0.7 EXPLAIN ANALYZE on demand

- Capital `E` triggers `EXPLAIN ANALYZE` — real wall-clock + actual row
  counts (vs. estimates).
- The parser recognises the `TABLE_SCAN` operator, `Total Time: X.Ys`,
  `Total Files Read: N`, `~N rows` (estimate) vs. plain `N rows`
  (actual).
- New heuristics: "estimate skewed" (actual / estimate > 100×),
  "scanned N files with no pushed predicate".

### 8.4 v0.8 async ANALYZE + query history

- **Async ANALYZE**: `E` returns immediately; the title bar ticks
  "running… 1.2 s"; `Esc` cancels (orphans the worker thread); the next
  query change auto-cancels the in-flight ANALYZE so stale results never
  back-fill. The worker thread holds its own DuckDB connection and ships
  results to the UI thread via `mpsc`.
- **Query history**: `Ctrl-↑` / `Ctrl-↓` in the Query pane scrolls
  `~/.pq/history` (max 100 entries, deduped, retyped queries auto-promote
  to top); entering history mode saves the current draft and `Ctrl-↓`
  past the bottom restores it.

### 8.5 Full TUI keymap

| Key                  | Action |
|----------------------|--------|
| `Tab` / `Shift-Tab`  | switch panes (Columns ↔ Query ↔ Data) |
| `↑↓` / `j k`         | move cursor (Columns row / Data row) |
| `← →`                | Data pane column cursor — drives semantic sync |
| `Space`              | in Columns: toggle projection |
| `Enter`              | add column / drill-down on Data row / accept completion |
| `Backspace`          | undo last drill-down |
| `:`                  | toggle SQL pane (see the SQL pq compiled) |
| `e`                  | toggle Explain pane (estimates + hints) |
| `E`                  | run `EXPLAIN ANALYZE` (async, `Esc` cancels) |
| `Ctrl-↑` / `Ctrl-↓`  | Query-pane history navigation |
| `?`                  | help overlay |
| `Q`                  | quit + print equivalent CLI to stdout |
| `Esc` / `q`          | quit (one Esc inside Query first defocuses) |
| `Ctrl-Y`             | copy equivalent CLI to clipboard |
| `Ctrl-C`             | force quit |

---

## 9. Testing & CI

### 9.1 Test layers

| Layer                          | Coverage | Count |
|--------------------------------|----------|-------|
| Unit tests (`#[test]`)         | parser, output, cloud, lineage, tui state | 109 |
| History bookkeeping (v0.8)     | `record_history` dedup / promote / cap   | 3 |
| TUI render snapshots (v0.8, `insta`) | empty / with-results / show-SQL / Explain (estimated / analyzed) / completion popup / help / error / drill-down | 9 |
| **Total**                      |          | **121** |

Snapshots are produced via `ratatui::backend::TestBackend`, normalising
rendered output to plain text (styles dropped) and stored in
`src/snapshots/`. Color tweaks don't break snapshots; layout / title /
status-bar wording changes do.

### 9.2 CI (GitHub Actions)

| Job                  | Trigger        | Content |
|----------------------|----------------|---------|
| `test (ubuntu)`      | PR / main push | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --release` |
| `test (macos)`       | same           | same |
| `tui smoke (vhs)`    | same (`continue-on-error: true`) | install vhs `.deb` (v0.11.0), run `assets/tui.tape`, drive the TUI in a PTY; only the process exit code is checked |

### 9.3 Release (tag-triggered)

| Job             | Content |
|-----------------|---------|
| `build`         | matrix: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-musl (via `cross`), x86_64-pc-windows-msvc. Each artifact ships with a `.sha256`. |
| `release`       | `softprops/action-gh-release` creates a GitHub Release with auto-generated changelog |
| `homebrew-bump` | uses a PAT to push to `thehwang/homebrew-parq`, regenerates `Formula/pq.rb` (with url+sha256 for all three platforms); pre-release tags (containing `-`) are skipped |

---

## 10. Installation

```bash
# One-liner installer (auto-detects macOS arm64 / x86_64 / Linux musl, drops into ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/thehwang/parq/main/install.sh | bash

# Homebrew tap
brew install thehwang/parq/pq
brew upgrade pq

# From source (needs Rust toolchain)
cargo install pq

# Windows: download .zip from the Releases page
```

---

## 11. Version history

| Version | Key deliverables |
|---------|------------------|
| v0      | single-stage DSL: projection / filter / nested paths; subcommands schema / stats / count / sample / head / tail |
| v0.2    | pipeline stages: `group_by`, aggregates, `top N by`, `sort by`, `limit`, `distinct`, `-o parquet` |
| v0.3    | INNER join, `--watch`, hive auto-detection |
| v0.4    | LEFT/RIGHT/FULL OUTER + multi-key join, `to_csv` / `to_json` line output, `--udf` macros, Homebrew tap, Windows binary |
| v0.5    | TUI MVP (Columns / Query / Filters / Data — four panes) |
| v0.5.1  | S3 `credential_chain` auto-discovery |
| v0.6    | semantic sync, schema completion, drill-down, Explain pane + heuristic hints |
| v0.7    | Homebrew auto-bump, `EXPLAIN ANALYZE` on demand |
| v0.8    | async ANALYZE (`Esc` cancels), persisted query history (`~/.pq/history`), 9 ratatui snapshot tests, VHS smoke test in CI |
| v0.9    | stdin auto-spool (`cat f.parquet \| pq -` Just Works), `-i / --input` formats (parquet / ndjson / csv, sniff by extension), pq becomes a true unix shell primitive |
| v0.9.1  | `to_ndjson` / `to_jsonl` aliases (unix-friendly names) |
| v0.10   | nested schema as a first-class citizen: LIST / STRUCT / MAP render as proper JSON (no more Rust Debug), jq-style path sugar (`.tags[0]` / `.tags[]` / `.events[0].kind` / `.metadata["plan"]`), `len` / `keys` / `values` builtins, JSON output preserves SELECT order (`preserve_order`) |
| **v0.11** | chained UNNEST: `.events[].kind` works in projection / WHERE / GROUP BY / HAVING / ORDER BY. The SQL compiler hoists every `UNNEST(...)` into a derived FROM, the outer query references `_pq_u<i>` aliases; same source dedup (`.events[].kind, .events[].amount` doesn't cartesian-explode); `alias_safe` strips UNNEST wrappers, aggregate alias goes from `sum_UNNEST_events__amount` → `sum_events_amount`; `head -n N` / `head -N` accept the unix flag forms; path errors (bad bracket, unclosed quote) now surface as pq's *invalid path* friendly error instead of DuckDB's *syntax error at or near `]`* |

---

## 12. Roadmap

- v0.12 candidates:
  - JOIN + chained UNNEST with finer-grained semantics (currently the
    hoister wraps the whole join in a derived table; could hoist only
    the side that needs unnesting to reduce output rows);
  - jq-style list predicate filters `.events[? .amount > 5]` →
    `list_filter(events, e -> e.amount > 5)`;
  - Interactive Filters pane (`d` delete, `e` edit) — not just
    read-only display;
  - `pq repl` (rustyline reusing `compile_plan`) — a readline entry
    point for engineers who don't like TUIs;
  - Fuzzy query-history search (`Ctrl-R` popup, reuses the completion
    popup renderer);
  - Excel `.xlsx` direct read (DuckDB excel extension); auto schema diff
    (compare two parquets, output markdown);
  - Explain pane: visualise DuckDB zonemap pruning (row-group min/max
    skipping);
  - TUI schema completion that recognises LIST / STRUCT / MAP and
    suggests `[`/`.` after column names, not just plain identifiers.
- Optional "true streaming" path: switch pq's output format from parquet
  to Arrow IPC stream so that `pq … | pq -i arrow -` becomes truly
  zero-copy / zero-spool — while keeping ndjson as the canonical
  chain lingua franca.
