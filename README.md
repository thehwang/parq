# `pq` — jq for Parquet

[![CI](https://github.com/thehwang/parq/actions/workflows/ci.yml/badge.svg)](https://github.com/thehwang/parq/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pq.svg)](https://crates.io/crates/pq)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Query parquet files with a concise expression syntax. Single binary, no JVM, no Python.

```bash
$ pq sales.parquet '.country, .revenue where .country == "US"'
┌─────────┬─────────┐
│ country ┆ revenue │
╞═════════╪═════════╡
│ US      ┆ 1245.00 │
│ US      ┆ 89.50   │
│ US      ┆ 17820.00│
└─────────┴─────────┘
(3 rows)
```

[ ![GIF — replace with your own asciinema cast](https://placehold.co/800x300?text=demo+GIF+goes+here) ](#)

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
# macOS / Linux (tap coming)
brew install thehwang/parq/pq

# from source
cargo install pq

# build from this repo
git clone https://github.com/thehwang/parq && cd parq
cargo build --release
./target/release/pq --help
```

## Quickstart

```bash
# Default: head 20 + schema overview
pq users.parquet

# Project: jq-style dot syntax (nested OK)
pq users.parquet '.email'
pq users.parquet '.user.id'
pq users.parquet '.email, .name, .country'
pq users.parquet 'select .email, .name'      # SQL-style alt

# Filter rows
pq users.parquet 'country == "US"'
pq users.parquet '.email where .country == "US"'

# Common ops
pq schema  users.parquet
pq stats   users.parquet
pq sample  users.parquet -n 10
pq head    users.parquet -n 5
pq count   users.parquet

# Cloud paths — DuckDB's httpfs handles auth via env vars
pq gs://bucket/file.parquet '.email'
pq s3://bucket/dt=2026-05-*/*.parquet 'count'

# Pipes — auto-switches to NDJSON for downstream tools
pq users.parquet '.email' | jq -r 'select(. | endswith("@cadent.tv"))'
pq users.parquet | head -3

# Output formats
pq users.parquet -o csv > out.csv
pq users.parquet -o json
pq users.parquet -o ndjson
pq users.parquet -o table

# Escape hatch: full DuckDB SQL when you need it
pq users.parquet 'SELECT country, count(*) FROM FILE GROUP BY country ORDER BY 2 DESC'
```

## Syntax

```
query        := projection
              | filter_expr
              | projection 'where' filter_expr
              | raw_sql                   -- starts with SELECT/WITH; FILE = the input
              | <empty>                   -- => head 20

projection   := ('select')? '.' ident ( ',' '.' ident )*
              | '.' ident ( '.' ident )*  -- nested struct path

filter_expr  := <DuckDB SQL fragment>     -- with sugar:
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

## What's coming

- [ ] Glob expansion: `pq 'data/dt=2026-*/*.parquet' 'count'`
- [ ] Aggregations sugar: `pq f.parquet 'group_by .country | count'`
- [ ] Sorting sugar: `pq f.parquet 'top 10 by .revenue'`
- [ ] Join sugar (multi-file): `pq a.parquet join b.parquet on .user_id`
- [ ] Watch mode: `pq -w f.parquet 'count'`
- [ ] Output to parquet: `pq a.parquet '.country == "US"' -o parquet > us.parquet`
- [ ] Partitioned hive scan with auto-discovery

## Limitations (v0)

- The "where" keyword in projection split is naive — quoted strings containing
  the literal text " where " inside them break parsing. Workaround: use
  the SQL passthrough.
- DuckDB embed adds ~30 MB to the binary. We accept the tradeoff for correctness.
- `-` (stdin) reads from `/dev/stdin` only; OS fifos / named pipes work but raw
  pipes from `cat` won't (DuckDB needs a seekable file).

## License

MIT
