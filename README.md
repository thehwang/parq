# `pq` — jq for Parquet

[![CI](https://github.com/thehwang/parq/actions/workflows/ci.yml/badge.svg)](https://github.com/thehwang/parq/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pq.svg)](https://crates.io/crates/pq)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Query parquet files with a concise expression syntax. Single binary, no JVM, no Python.

> **New in v0.6:** the TUI just grew teeth. Cursor on `sum_revenue` highlights the source `revenue` in the Columns panel (semantic sync); typing `.c` pops up a schema completion menu; `Enter` on any Data row drills down by adding a `where` clause; press `e` for an Explain panel with predicate/projection-pushdown facts and 💡 actionable suggestions. See [Interactive TUI](#interactive-tui-pq-tui-file) below.
>
> _v0.5 added `pq tui FILE` — interactive lazygit-style 4-panel browser with live preview, editable DSL, and equivalent-CLI export on quit._

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
# One-liner (auto-detects macOS arm64/x86_64 + Linux musl, installs to ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/thehwang/parq/main/install.sh | bash

# Homebrew
brew install thehwang/parq/pq

# Prebuilt binary, manual (replace asset name for your platform)
curl -fsSL https://github.com/thehwang/parq/releases/latest/download/pq-aarch64-apple-darwin.tar.gz \
  | tar xz && sudo mv pq /usr/local/bin/

# Windows: download .zip from the Releases page
#   https://github.com/thehwang/parq/releases/latest

# From source
cargo install pq

# Build from this repo
git clone https://github.com/thehwang/parq && cd parq
cargo build --release && ./target/release/pq --help
```

Available prebuilt assets per release: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-musl` (works on every Linux), `x86_64-pc-windows-msvc` (.zip).
Each tarball/zip ships with a `.sha256` sidecar.

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
| `[inner\|left\|right\|full]_join "f" on …` | `left_join "o.parquet" on .id` | `LEFT OUTER JOIN read_parquet('o.parquet') AS b …` |
| `to_csv` / `to_json` | `.email, .country \| to_csv` | wraps in `concat_ws(',', …)` / `to_json({…})` |

### Subcommands

```bash
pq schema  users.parquet     # column names + types + nullable
pq stats   users.parquet     # min, max, approx_distinct, null_pct per col
pq sample  users.parquet -n 10
pq head    users.parquet -n 5
pq tail    users.parquet -n 5
pq count   users.parquet
pq tui     users.parquet     # interactive 4-panel browser (see below)
```

### Interactive TUI (`pq tui FILE`)

Lazygit-style 4-panel browser for exploring a parquet file without leaving
the terminal:

```
┌─ Columns · 5 ──────────┐ ┌─ Query · 2 ms ──────────────────────────┐
│ ✓ id        BIGINT     │ │ group_by .country | sum .revenue        │
│ ✓ email     VARCHAR    │ │                                         │
│ ✓ country   VARCHAR    │ └─────────────────────────────────────────┘
│ ▶ revenue   DOUBLE     │ ┌─ Data · 7 of 7 rows shown ─────────────┐
│   age       BIGINT     │ │ country │ sum_revenue                  │
│                        │ │ US      │ 19065.00                     │
└────────────────────────┘ │ FR      │   999.99                     │
┌─ Filters · 1 ──────────┐ │ DE      │   312.00                     │
│ • .country == "US"     │ │ ...                                    │
└────────────────────────┘ └────────────────────────────────────────┘
 Tab next │ ␣ toggle col │ ⏎ append │ Q exit+print │ Esc/q quit │ : SQL │ ?
```

The Query panel is the source of truth: edit your DSL there, watch the
Data panel re-run live (50 ms throttle), peek at compiled SQL with `:`,
and exit with `Q` to dump the equivalent `pq` CLI one-liner to stdout
— so the TUI doubles as a query builder for your shell history.

#### v0.6 super-powers

- **Semantic sync.** Park your cursor on `sum_revenue` in the Query panel
  and `★ revenue` lights up gold in the Columns panel. Move the Data
  panel cursor onto a column header — same trick, source field highlights
  back in Columns. Lineage is forgiving: works on partial / mid-typing
  queries too.
- **Schema completion.** Type `.c` in the Query panel and a popup of
  matching columns appears (prefix wins; substring matches still shown).
  `Tab` / `↑↓` to cycle, `Enter` to accept, `Esc` to dismiss.
- **Drill-down.** Run `group_by .country | count`, focus the Data panel,
  arrow down to the `US` row, hit `Enter` — the Query panel grows a
  `where .country == "US"` clause and re-runs. `Backspace` undoes the
  drill if you went too deep.
- **Explain panel (`e`).** Pops a band under Data showing how DuckDB
  plans your query: scans, predicate pushdown, projection pushdown, and
  💡 heuristic hints when something obvious is missing —
  e.g. `💡 add 'where .dt = …' to prune partitions` if your path has
  hive segments but the query doesn't reference them.

Keys at a glance (full list inside the TUI via `?`):

| key | what it does |
|---|---|
| `Tab` / `Shift-Tab` | cycle focus across panels |
| `↑↓` / `j k` | move cursor (Columns / Data row) |
| `← →` | move column cursor in Data panel (drives semantic sync) |
| `Space` | toggle column in projection (Columns panel) |
| `Enter` | append column / drill down on Data row / accept completion |
| `Backspace` | undo last drill-down (Data panel) |
| `:` | toggle compiled-SQL panel |
| `e` | toggle Explain panel (pushdown facts + 💡 hints) |
| `?` | open help overlay (any key dismisses) |
| `Q` | quit + print equivalent CLI |
| `Esc` / `q` | quit; one Esc inside Query unfocuses first |
| `Ctrl-C` | force quit through any modal |

### Cloud paths, globs, hive auto-discovery

DuckDB's `read_parquet` handles all of these natively. pq auto-loads the
`httpfs` extension and reads cloud credentials from environment variables —
no need to drop into the DuckDB CLI to `CREATE SECRET`:

| env vars                                              | creates                                |
|-------------------------------------------------------|----------------------------------------|
| `PQ_GCS_BEARER_TOKEN`                                 | GCS OAuth secret — recommended for interactive use |
| `PQ_GCS_HMAC_KEY` + `PQ_GCS_HMAC_SECRET`              | GCS HMAC secret — long-lived, for cron / batch     |
| `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`         | S3 secret (also reads `AWS_SESSION_TOKEN`, `AWS_REGION`, `AWS_ENDPOINT_URL_S3`) |
| _(none of the above for S3)_                          | falls back to `credential_chain` — auto-resolves `AWS_PROFILE`, `~/.aws/credentials`, SSO, EC2 IMDS, ECS task role |

```bash
# GCS — OAuth (interactive, easiest; token refreshes ~hourly via gcloud)
export PQ_GCS_BEARER_TOKEN=$(gcloud auth print-access-token)
pq schema gs://bucket/file.parquet

# GCS — HMAC (long-lived, cron-friendly, no expiry)
export PQ_GCS_HMAC_KEY='GOOG1XXXXXX...'    # from `gcloud storage hmac create`
export PQ_GCS_HMAC_SECRET='...'
pq gs://bucket/file.parquet '.email'

# S3 — explicit env vars
export AWS_ACCESS_KEY_ID=AKIA…
export AWS_SECRET_ACCESS_KEY=…
pq s3://my-bucket/file.parquet | head

# S3 — named profile from ~/.aws/credentials (no env vars needed)
export AWS_PROFILE=cadent-prod
pq schema s3://my-bucket/file.parquet

# S3 — SSO  (works once `aws sso login` cached a token)
aws sso login --profile=cadent-sso
AWS_PROFILE=cadent-sso pq schema s3://my-bucket/file.parquet

# S3 — IAM role on EC2 / ECS  (no creds anywhere — chain pulls from IMDS / task role)
pq s3://my-bucket/file.parquet

# Globs (quote them so the shell doesn't expand first)
pq 'data/dt=2026-*/*.parquet' 'group_by .dt | count'
```

**Auto-refresh trick** — drop in your `~/.zshrc` so every new shell gets a
fresh token without thinking about it:

```bash
pq() {
  if [[ -z "$PQ_GCS_BEARER_TOKEN" ]] && command -v gcloud >/dev/null 2>&1; then
    export PQ_GCS_BEARER_TOKEN=$(gcloud auth print-access-token 2>/dev/null)
  fi
  command pq "$@"
}
```

Set `PQ_DEBUG=1` to see which secret got registered (otherwise pq stays
quiet — credential noise has no place on stdout).

**Hive partitioning auto-detects.** Any path containing a `key=value` segment
turns the partition keys into normal columns you can group/filter on:

```bash
# 'sales/dt=2026-05-20/region=US/part-0.parquet' — pq sees dt + region columns automatically
pq 'sales/dt=*/region=*/*.parquet' 'group_by .dt, .region | count | sum .amount'
```

### Joins

INNER (default), LEFT / RIGHT / FULL OUTER — pick the verb that matches what
you'd write in SQL. Left side is `a`, right side is `b` (referenced as
`.a.col` / `.b.col` in subsequent stages):

```bash
# INNER (shorthand: same column name on both sides)
pq orders.parquet 'join "users.parquet" on .user_id | select .a.amount, .b.email'

# Explicit ON expression — different column names per side
pq users.parquet 'join "orders.parquet" on .a.id == .b.user_id \
                  | group_by .a.country | sum .b.amount | sort by .sum_b_amount desc'

# LEFT OUTER — keep all users, even ones with no orders (b.* is ∅)
pq users.parquet 'left_join "orders.parquet" on .a.id == .b.user_id \
                  | select .a.email, .b.amount, .b.status'

# Multi-key — just compose with `and`
pq users.parquet 'inner_join "events.parquet" \
                    on .a.id == .b.user_id and .a.dt == .b.dt | count'
```

`right_join` and `full_join` (alias `outer_join`) work identically. The right
side supports cloud URIs and hive auto-discovery the same as the left.

### Line output: `to_csv` / `to_json`

Two stages that fold each row into a single TEXT line. No headers, no quoting,
no JSON wrapping — what stdout sees is what `awk` / `jq` / `xsv` consume:

```bash
# Raw CSV per row, no header
pq users.parquet '.email, .country, .revenue | to_csv'
# alice@example.com,US,1245.0
# bob@example.com,CA,89.5
# …

# JSON object per row (stable field names — even after group_by/agg)
pq users.parquet 'group_by .country | sum .revenue | to_json' \
  | jq -r 'select(.sum_revenue > 1000) | .country'

# `to_json` with no projection dumps the whole row as a struct
pq users.parquet 'where .age > 18 | to_json' | jq .
```

Internally these wrap your pipeline in a subquery so `sort by` / `limit`
upstream still work as expected.

### `--udf`: register DuckDB SQL macros

Define helpers once, reuse across stages. Repeatable. The `:=` is rewritten
to DuckDB's `CREATE OR REPLACE MACRO ... AS ...` automatically:

```bash
pq sample.parquet \
  --udf $'is_us(c) := c = \'US\'' \
  --udf 'discount(x) := x * 0.9' \
  '.email, discount(.revenue) AS d where is_us(.country) | sort by .d desc'
```

For one-off needs you can also just call DuckDB's built-ins directly inside
`where` / `select` — `regexp_matches`, `list_contains`, `to_timestamp`, etc.

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

**v0.5.1** (current)
- [x] S3 `credential_chain` fallback — `AWS_PROFILE`, `~/.aws/credentials`, SSO,
      EC2 IMDS, and ECS task role now Just Work without setting `AWS_ACCESS_KEY_ID`

**v0.5**
- [x] `pq tui FILE` — interactive 4-panel browser (Columns / Filters / editable Query / live Data)
- [x] Editable DSL panel as the source of truth, throttled live preview (50 ms)
- [x] Ghost-text placeholder, visible block cursor when focused, `Esc/q` quits
- [x] `Space` toggles a column, `Enter` appends, `?` opens full help overlay
- [x] Compiled SQL hidden behind `:` (DSL-first, SQL on demand)
- [x] Auto-loads `httpfs` + creates DuckDB secrets from env vars: `PQ_GCS_BEARER_TOKEN`, `PQ_GCS_HMAC_*`, `AWS_*` — no more `duckdb -c CREATE SECRET` dance
- [x] Numeric columns right-align with cyan headers; long values get `…` truncation marker

**v0.4**
- [x] LEFT / RIGHT / FULL OUTER joins (alongside INNER)
- [x] Multi-key joins: `'join "b" on .a.x == .b.x and .a.y == .b.y'`
- [x] `to_csv` / `to_json` line-output stages — raw line per row, no headers
- [x] `--udf` flag — register DuckDB SQL macros before the query runs
- [x] Windows binary (`x86_64-pc-windows-msvc.zip`) on every release
- [x] Homebrew tap: `brew install thehwang/parq/pq`
- [x] One-line installer: `curl -fsSL …/install.sh | bash`

**v0.3**
- [x] Hive partitioning auto-detects from `key=value` path segments — no flag needed
- [x] Single equi-join: `'join "b.parquet" on .col'` and `'join "b.parquet" on .a.x == .b.y'`
- [x] Watch mode: `pq -w 5 file.parquet 'count'` re-runs every N seconds
- [x] Date32 columns display as `YYYY-MM-DD`
- [x] Prebuilt binaries on every tag — macOS arm64/x86_64 + Linux musl

**v0.2**
- [x] Glob expansion: `pq 'data/dt=2026-*/*.parquet' 'count'`
- [x] Aggregation sugar: `group_by`, `count`, `sum/avg/min/max`, `count_distinct`
- [x] Sorting sugar: `top N by .col`, `sort by .col [desc]`
- [x] Output to parquet: `pq a.parquet 'where .country == "US"' -o parquet > us.parquet`
- [x] Pipe stages with WHERE/HAVING auto-routing

## What's coming

**v0.6 — TUI depth pass**
- [ ] Semantic cursor sync — column lineage highlighting across all panels
- [ ] `Y` truly copies to clipboard (arboard with feature flag for headless builds)
- [ ] Horizontal scroll in Data panel for long-string columns
- [ ] Real-time row count (currently shows "preview rows", not full count)
- [ ] `Filters` panel becomes interactive (drop a filter with `d`, edit with `e`)
- [ ] Explain panel with `EXPLAIN ANALYZE` + heuristic hints (zonemap pruning, projection PD)
- [ ] Drill-down: Enter on an aggregate cell → auto-generates `where` for underlying rows
- [ ] DuckDB GCS `credential_chain` ADC support once duckdb#22413 lands

**v0.7+**
- [ ] Query history with branching (every keystroke a frame, rewindable)
- [ ] Schema diff between two parquet files
- [ ] Multi-file tabs with visual join builder

## Limitations (v0)

- The "where" keyword in projection split is naive — quoted strings containing
  the literal text " where " inside them break parsing. Workaround: use
  the SQL passthrough.
- DuckDB embed adds ~30 MB to the binary. We accept the tradeoff for correctness.
- `-` (stdin) reads from `/dev/stdin` only; OS fifos / named pipes work but raw
  pipes from `cat` won't (DuckDB needs a seekable file).

## License

MIT
