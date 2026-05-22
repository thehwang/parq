# `pq` — jq for Parquet

[![CI](https://github.com/thehwang/parq/actions/workflows/ci.yml/badge.svg)](https://github.com/thehwang/parq/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pq.svg)](https://crates.io/crates/pq)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Query parquet files with a concise expression syntax. Single binary, no JVM, no Python.

```bash
$ pq sales.parquet 'group_by .country | sum .revenue | top 3 by sum_revenue'
┌─────────┬─────────────┐
│ country ┆ sum_revenue │
╞═════════╪═════════════╡
│ US      ┆ 19065.00    │
│ FR      ┆ 999.99      │
│ DE      ┆ 312.00      │
└─────────┴─────────────┘
(3 rows)
```

![demo](assets/demo.gif)

## Why?

The current options for ad-hoc parquet querying are all painful:

| Tool | Pain |
|---|---|
| `pyarrow` / `pandas` | 5 second cold start, 200MB virtualenv |
| `parquet-tools` (Apache) | JVM, slow, no query support |
| `pqrs` | Inspector only — can't filter or project |
| `duckdb` CLI | Great engine, but `SELECT email FROM 'file.parquet' WHERE country='US'` is too verbose for one-liners |
| Spark | Are you serious |

`pq` is a 50 MB single binary that wraps DuckDB's query engine in a `jq`-style syntax optimized for terminal one-liners and pipes.

## Install

```bash
# Prebuilt binary — macOS arm64 / x86_64 / Linux musl
# (replace the asset name to match your platform; SHA256 sidecars are next to each archive)
curl -L https://github.com/thehwang/parq/releases/latest/download/pq-aarch64-apple-darwin.tar.gz \
  | tar xz && sudo mv pq /usr/local/bin/

# from source
cargo install pq

# build from this repo
git clone https://github.com/thehwang/parq && cd parq
cargo build --release
./target/release/pq --help
```

## Quickstart

### Single-stage (the v0 way — still supported)

```bash
pq users.parquet                                  # head 20
pq users.parquet '.email'                         # one column
pq users.parquet '.user.id'                       # nested struct path
pq users.parquet '.email, .name, .country'        # multi
pq users.parquet 'country == "US"'                # filter only
pq users.parquet '.email where .country == "US"'  # both
```

### Pipe stages (the killer feature)

Stages are separated by `|`. Output of one stage flows into the next.

```bash
# Top countries by revenue
pq sales.parquet 'group_by .country | sum .revenue | top 10 by sum_revenue'

# Filter, group, having
pq users.parquet 'where .age > 18 | group_by .country | count | where count > 100'

# Distinct values
pq logs.parquet '.user_id | distinct | sort by .user_id'

# Multi-aggregate
pq events.parquet 'group_by .country | count | sum .duration | avg .duration'
```

| Verb | Example | SQL emitted |
|---|---|---|
| `where EXPR` | `where .age > 18` | `WHERE age > 18` (or `HAVING` after group_by) |
| `select .col, .col2` | `select .email, .name` | `SELECT email, name` |
| `group_by .col[, .col2]` | `group_by .country` | `GROUP BY country` |
| `count` / `count_distinct .col` | `count_distinct .npi` | `count(DISTINCT npi) AS count_distinct_npi` |
| `sum/avg/min/max .col` | `sum .revenue` | `sum(revenue) AS sum_revenue` |
| `top N by COL [asc\|desc]` | `top 10 by sum_revenue` | `ORDER BY sum_revenue DESC LIMIT 10` |
| `sort by .col [asc\|desc]` | `sort by .revenue desc` | `ORDER BY revenue DESC` |
| `limit N` / `head N` | `limit 5` | `LIMIT 5` |
| `distinct` | `distinct` | `SELECT DISTINCT` |

### Subcommands

```bash
pq schema  users.parquet     # column names + types + nullable
pq stats   users.parquet     # min, max, approx_distinct, null_pct per col
pq sample  users.parquet -n 10
pq head    users.parquet -n 5
pq tail    users.parquet -n 5
pq count   users.parquet
```

### Cloud paths, globs, hive auto-discovery

DuckDB's `read_parquet` handles all of these natively:

```bash
pq gs://bucket/file.parquet '.email'

# Globs (quote them so the shell doesn't expand first)
pq 'data/dt=2026-*/*.parquet' 'group_by .dt | count'
```

**Hive partitioning auto-detects.** Any path containing a `key=value` segment
turns the partition keys into normal columns you can group/filter on:

```bash
# 'sales/dt=2026-05-20/region=US/part-0.parquet' — pq sees dt + region columns automatically
pq 'sales/dt=*/region=*/*.parquet' 'group_by .dt, .region | count | sum .amount'
```

### Joins

Single equi-join, two shapes — bare `.col` for matching column names, explicit
`.a.x == .b.y` otherwise. Left side is `a`, right side is `b`:

```bash
# Same column name on both sides
pq orders.parquet 'join "users.parquet" on .user_id | select .a.amount, .b.email'

# Different column names (users.id ↔ orders.user_id)
pq users.parquet 'join "orders.parquet" on .a.id == .b.user_id \
                  | group_by .a.country | count | sum .b.amount | sort by .sum_b_amount desc'
```

The right side also supports cloud URIs and hive auto-discovery.

### Watch mode

Re-runs the query every N seconds with a screen-clear between ticks. Drop it
on a directory that's actively being written to:

```bash
pq -w 5 'data/dt=2026-*/*.parquet' 'group_by .dt | count | sort by .dt desc | limit 5'
```

`Ctrl-C` to stop. The status line on stderr reports the tick count + elapsed
time so you can tell it's alive.

### Pipe-friendly

`pq` auto-detects whether stdout is a TTY:

```bash
pq users.parquet '.email' | jq -r 'select(endswith("@example.org"))'
pq users.parquet | head -3
```

### Output formats

```bash
pq users.parquet -o csv > out.csv
pq users.parquet -o json
pq users.parquet -o ndjson

# Export back to parquet (auto-disables default LIMIT)
pq big.parquet 'where .country == "US"' -o parquet > us.parquet
```

### Escape hatch

When the DSL doesn't cover what you need, drop into raw SQL:

```bash
pq users.parquet 'SELECT country, count(*) FROM FILE GROUP BY country ORDER BY 2 DESC'
# `FILE` is substituted with read_parquet('users.parquet')
```

## Grammar

```
query        := stage ( '|' stage )*
              | raw_sql                          -- starts with SELECT/WITH
              | <empty>                          -- => head LIMIT n

stage        := projection                       -- '.col, .col2'
              | filter_expr                      -- 'country == "US"'
              | projection 'where' filter_expr   -- v0 inline shorthand
              | 'where' filter_expr
              | 'select' projection
              | 'group_by' '.' ident (',' '.' ident)*
              | 'count'
              | ('sum'|'avg'|'min'|'max'|'count_distinct') '.' ident
              | 'top' INT 'by' col [ asc | desc ]
              | 'sort by' col [ asc | desc ]
              | 'limit' INT
              | 'distinct'
              | 'join' '"' path '"' 'on' join_clause   -- v0.3
join_clause  := '.' ident                              -- shorthand: a.col = b.col
              | filter_expr                            -- explicit, must contain '='

filter_expr  := <DuckDB SQL fragment>            -- with sugar:
                  "..."   → '...'  (jq strings → SQL string literals)
                  ==      → =
                  !=      → <>
                  bare .col → col
```

## Comparison

```
                       pq      duckdb-cli   pqrs   pyarrow   parquet-tools
size (single binary)  50 MB    24 MB        5 MB   ~200 MB   N/A (JVM)
cold start            ~50 ms   ~80 ms       ~10 ms ~5 sec    ~5 sec
filter / project      ✓        ✓ (verbose)  ✗      ✓         ✗
group_by / agg        ✓        ✓            ✗      ✓         ✗
gs:// / s3:// paths   ✓        ✓            ~      manual    ✗
nested column access  ✓        ✓            ✗      ✓         ~
schema dump           ✓        ✓            ✓      ✓         ✓
streams large files   ✓        ✓            ✓      partial   ✓
```

## What's done

**v0.3** (current)
- [x] Hive partitioning auto-detects from `key=value` path segments — no flag needed
- [x] Single equi-join: `'join "b.parquet" on .col'` and `'join "b.parquet" on .a.x == .b.y'`
- [x] Watch mode: `pq -w 5 file.parquet 'count'` re-runs every N seconds
- [x] Date32 columns now display as `YYYY-MM-DD` (not `date(days=20592)`)
- [x] Prebuilt binaries on every tag — macOS arm64/x86_64 + Linux musl

**v0.2**
- [x] Glob expansion: `pq 'data/dt=2026-*/*.parquet' 'count'`
- [x] Aggregation sugar: `group_by`, `count`, `sum/avg/min/max`, `count_distinct`
- [x] Sorting sugar: `top N by .col`, `sort by .col [desc]`
- [x] Output to parquet: `pq a.parquet 'where .country == "US"' -o parquet > us.parquet`
- [x] Pipe stages with WHERE/HAVING auto-routing

## What's coming (v0.4+)

- [ ] LEFT/RIGHT/FULL OUTER joins (currently INNER only)
- [ ] Multi-key join: `'join "b.parquet" on .a.x == .b.x and .a.y == .b.y'`
- [ ] `to_csv .col` / `to_json` per-row output sugar
- [ ] Scalar UDFs: `pq f.parquet 'where regex_match(.email, "@example\\.org")'`
- [ ] Homebrew tap (`brew install thehwang/parq/pq`)

## Limitations (v0)

- The "where" keyword in projection split is naive — quoted strings containing
  the literal text " where " inside them break parsing. Workaround: use
  the SQL passthrough.
- DuckDB embed adds ~30 MB to the binary. We accept the tradeoff for correctness.
- `-` (stdin) reads from `/dev/stdin` only; OS fifos / named pipes work but raw
  pipes from `cat` won't (DuckDB needs a seekable file).

## License

MIT
