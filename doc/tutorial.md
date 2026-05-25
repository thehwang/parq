# `pq` Tutorial — From Zero to Productive in 30 Minutes

> This tutorial assumes you **know a little SQL or jq** (no expertise needed)
> and have never used `pq` before. Type along, command by command. By the
> end you'll be writing real business queries, knowing when to switch to
> the TUI, and knowing when to drop into raw SQL.
>
> Need to look up a specific feature later? See [`reference.md`](./reference.md) —
> the full feature catalogue, dictionary-style.

---

## Table of Contents

- [Setup (5 min)](#setup-5-min)
- [Lesson 1: The DSL pipeline mental model (5 min)](#lesson-1-the-dsl-pipeline-mental-model-5-min)
- [Lesson 2: Nested types (10 min — pq's killer feature)](#lesson-2-nested-types-10-min--pqs-killer-feature)
- [Lesson 3: Composing with the unix toolbox (5 min)](#lesson-3-composing-with-the-unix-toolbox-5-min)
- [Lesson 4: TUI in practice (5 min)](#lesson-4-tui-in-practice-5-min)
- [Lesson 5: Big-file mode (5 min — v0.12 + v0.13)](#lesson-5-big-file-mode-5-min--v012--v013)
- [Cheat sheet: common idioms](#cheat-sheet-common-idioms)
- [Next steps](#next-steps)

---

## Setup (5 min)

### Install pq

```bash
# macOS / Linux one-liner (auto-detects arm64 / x86_64 / Linux musl)
curl -fsSL https://raw.githubusercontent.com/thehwang/parq/main/install.sh | bash

# Or via Homebrew
brew install thehwang/parq/pq

# Verify
pq --version
# pq 0.13.0
```

### Build a sample parquet

We'll use a fictional e-commerce events file throughout. Easiest way to
generate it is via DuckDB's CLI (install with `brew install duckdb` if
you don't have it — it's a 10-second one-liner):

```bash
duckdb -c "
COPY (
  SELECT
    id AS user_id,
    ['US','UK','DE','FR'][((id - 1) % 4) + 1] AS country,
    round((random() * 100)::DOUBLE, 2) AS revenue,
    TIMESTAMP '2026-01-01' + INTERVAL (id) HOUR AS ts,
    [
      {'kind': 'click', 'amount': round((random() * 5)::DOUBLE, 2)},
      {'kind': CASE WHEN id % 3 = 0 THEN 'buy' WHEN id % 5 = 0 THEN 'refund' ELSE 'view' END,
       'amount': round((random() * 100)::DOUBLE, 2)}
    ] AS events,
    MAP(['plan', 'seat'],
        [CASE WHEN id % 5 = 0 THEN 'pro' ELSE 'free' END, (id % 10)::VARCHAR]) AS metadata
  FROM range(1, 1001) t(id)
) TO '/tmp/events.parquet' (FORMAT PARQUET);"

ls -lh /tmp/events.parquet
# -rw-r--r--  1 you  staff   ~30K
```

What's in the file: 1000 e-commerce event rows, with all three Parquet
nested types — `LIST<STRUCT>` (`events`), `MAP<VARCHAR,VARCHAR>`
(`metadata`), plus regular scalars.

### First command

```bash
pq /tmp/events.parquet
```

That prints the first 20 rows as a table. **First command, no query, no
flags, no config** — that's the baseline experience.

---

## Lesson 1: The DSL pipeline mental model (5 min)

> **Key idea**: a pq query is a chain of stages joined by `|`, each stage a
> small jq/SQL-style operation. pq compiles the chain to a single DuckDB
> SQL statement and executes it.

### 1.1 Inspect the schema

```bash
pq schema /tmp/events.parquet
# user_id     INTEGER     YES
# country     VARCHAR     YES
# revenue     DOUBLE      YES
# ts          TIMESTAMP   YES
# events      STRUCT(kind VARCHAR, amount DOUBLE)[]   YES
# metadata    MAP(VARCHAR, VARCHAR)   YES
```

The last two rows are the nested types — Lesson 2 covers them in depth.

### 1.2 Project + filter (v0 inline form)

```bash
pq /tmp/events.parquet '.user_id, .country, .revenue where .country == "US"'
# {"user_id":1,"country":"US","revenue":12.34}
# {"user_id":5,"country":"US","revenue":78.9}
# ...
```

Three things in one line:

- `.user_id, .country, .revenue` — project three columns (jq-style paths)
- `where .country == "US"` — filter
- pq compiles this to `SELECT user_id, country, revenue FROM read_parquet('/tmp/events.parquet') WHERE country = 'US' LIMIT 20`

### 1.3 Same query, pipeline form

```bash
pq /tmp/events.parquet 'where .country == "US" | .user_id, .country, .revenue'
```

Equivalent. **`|` is pq's stage separator** — distinct from the shell pipe
(the entire query lives in single quotes).

When to use which? Inline is shorter; the pipeline scales. **Anything
multi-stage uses pipelines**:

```bash
pq /tmp/events.parquet '
  where .country == "US"
  | group_by .country
  | sum .revenue
  | top 5 by sum_revenue
'
```

### 1.4 See the SQL pq compiled

```bash
pq /tmp/events.parquet 'where .country == "US" | group_by .country | sum .revenue' --explain
# SELECT country, sum(revenue) AS sum_revenue
# FROM read_parquet('/tmp/events.parquet')
# WHERE country = 'US'
# GROUP BY country
```

`--explain` compiles without executing. **Whenever you're not sure what
pq is doing, add `--explain`** — it's the single most efficient debugging
tool.

### 1.5 Output formats

The default is a table on TTY, ndjson when piped. You can also force one:

```bash
pq /tmp/events.parquet '.user_id, .country | head 3' -o table
# ┌─────────┬─────────┐
# │ user_id │ country │
# ├─────────┼─────────┤
# │       1 │ US      │
# │       2 │ UK      │
# │       3 │ DE      │
# └─────────┴─────────┘

pq /tmp/events.parquet '.user_id, .country | head 3' -o json
# [{"user_id":1,"country":"US"},{"user_id":2,"country":"UK"},{"user_id":3,"country":"DE"}]

pq /tmp/events.parquet '.user_id, .country | head 3' -o csv
# user_id,country
# 1,US
# 2,UK
# 3,DE
```

`-o ndjson` / `-o parquet` exist too — see [reference §7](./reference.md#7-output-formats) for the full list.

### Lesson 1 checkpoint

```bash
# Your turn: top 3 UK rows by revenue (descending)
pq /tmp/events.parquet 'where .country == "UK" | sort by .revenue desc | head 3'
```

Got rows back? On to Lesson 2.

---

## Lesson 2: Nested types (10 min — pq's killer feature)

> **Key idea**: Parquet's three nested types — `STRUCT` / `LIST` / `MAP` —
> all have jq-style sugar in pq, and the v0.11 chained-UNNEST hoister means
> you almost never need to drop into raw SQL.

### 2.1 Path syntax cheatsheet

| pq syntax | meaning |
|---|---|
| `.user.email` | STRUCT field |
| `.tags[0]` | LIST first element (**jq is 0-indexed**, pq translates to DuckDB's 1-indexed) |
| `.tags[-1]` | LIST last element |
| `.tags[]` | row explosion (UNNEST) — turn each element into its own row |
| `.events[0].kind` | LIST<STRUCT> — index then field |
| `.events[].kind` | row explosion + field (v0.11+; covered below) |
| `.metadata["plan"]` | MAP key lookup (single or double quotes) |
| `len(.tags)` | LIST length |
| `keys(.metadata)` | MAP keys |
| `values(.metadata)` | MAP values |

### 2.2 STRUCT — just dot through

```bash
# Our events column has nested STRUCT fields
pq /tmp/events.parquet '.events[0].kind, .events[0].amount | head 3'
# {"events_0_kind":"click","events_0_amount":2.31}
# {"events_0_kind":"click","events_0_amount":4.10}
# {"events_0_kind":"click","events_0_amount":1.85}
```

Note the auto-aliased `events_0_kind` — that's v0.10's snake_case alias
for bracket-bearing paths, so JSON keys never expose SQL internals like
`(events[1]).kind`.

### 2.3 LIST — index, negative index, length

```bash
# First event vs last event
pq /tmp/events.parquet '.user_id, .events[0].kind, .events[-1].kind | head 3'
# {"user_id":1,"events_0_kind":"click","events_neg1_kind":"view"}

# Array length
pq /tmp/events.parquet '.user_id, len(.events) | head 3'
# {"user_id":1,"len_events":2}
```

### 2.4 Row explosion (UNNEST) — pq's signature move

```bash
# Explode events — each original row becomes N rows
pq /tmp/events.parquet '.user_id, .events[].kind | head 5'
# {"user_id":1,"events_kind":"click"}
# {"user_id":1,"events_kind":"view"}      ← user_id=1 became two rows
# {"user_id":2,"events_kind":"click"}
# {"user_id":2,"events_kind":"view"}
# {"user_id":3,"events_kind":"click"}
```

**The v0.11 upgrade**: `.events[].kind` (explode then take a field) used
to require raw SQL. Now it works in projection, `where`, `group_by`,
`sort by` — every clause.

### 2.5 Real-world: count events by kind

```bash
pq /tmp/events.parquet 'group_by .events[].kind | count | sort by .count desc'
# {"events_kind":"click","count":1000}
# {"events_kind":"view","count":466}
# {"events_kind":"buy","count":333}
# {"events_kind":"refund","count":201}
```

Add `--explain` to see what pq did:

```bash
pq /tmp/events.parquet 'group_by .events[].kind | count' --explain
# SELECT _pq_u0.kind AS events_kind, count(*) AS count
# FROM (SELECT *, UNNEST(events) AS _pq_u0
#       FROM read_parquet('/tmp/events.parquet')) AS _pq_src
# GROUP BY _pq_u0.kind
```

pq lifted `UNNEST(events)` into a derived table; the outer SELECT only
sees a regular STRUCT column `_pq_u0`. That's the v0.11 chained-UNNEST
hoister at work.

### 2.6 MAP — key lookup

```bash
# Each user's plan
pq /tmp/events.parquet '.user_id, .metadata["plan"] | head 5'
# {"user_id":1,"metadata_plan":"free"}
# {"user_id":2,"metadata_plan":"free"}
# {"user_id":3,"metadata_plan":"free"}
# {"user_id":4,"metadata_plan":"free"}
# {"user_id":5,"metadata_plan":"pro"}

# Total revenue from pro users
pq /tmp/events.parquet 'where .metadata["plan"] == "pro" | sum .revenue'
# {"sum_revenue":10234.5}

# All keys in each MAP
pq /tmp/events.parquet 'keys(.metadata) | head 3'
# {"keys_metadata":["plan","seat"]}
```

### 2.7 Shared-source dedup (an important performance contract)

```bash
# Same events list referenced twice — pq dedupes the UNNEST,
# you get N rows not N*N
pq /tmp/events.parquet '.user_id, .events[].kind, .events[].amount | head 5'
```

Add `--explain` and you'll see exactly one `UNNEST(events) AS _pq_u0` in
the inner subquery — both outer references share it. Without this
contract, writing `events[].kind, events[].amount` would naively look
like two unnests; pq dedupes for you.

### Lesson 2 checkpoint

```bash
# Your turn: top 3 countries by total amount of "buy" events
pq /tmp/events.parquet '
  where .events[].kind == "buy"
  | group_by .country
  | sum .events[].amount
  | top 3 by sum_events_amount
'
```

Got rows back? On to Lesson 3.

---

## Lesson 3: Composing with the unix toolbox (5 min)

> **Key idea**: pq turns parquet into a shell primitive. Read stdin, write
> stdout, compose freely with jq / awk / xsv / curl.

### 3.1 Output ndjson for downstream tools

```bash
# pq → ndjson, jq filters by timestamp, fed back into pq for counting
pq /tmp/events.parquet '.user_id, .country, .ts | to_json' \
  | jq -c 'select(.ts >= "2026-01-15")' \
  | pq -i ndjson - 'count'
# {"count":408}
```

The `to_json` stage makes pq emit one JSON object per line (ndjson), and
`-i ndjson -` tells the next pq invocation to read ndjson from stdin.

### 3.2 Pipe parquet directly (v0.9+)

```bash
# stdin can be parquet too — pq spools to a tempfile (DuckDB's reader needs a seekable fd)
cat /tmp/events.parquet | pq - 'group_by .country | sum .revenue'

# Direct from cloud storage — no local download
# aws s3 cp s3://bucket/file.parquet - | pq - 'group_by .x | count'
```

### 3.3 Multi-stage pq relays

```bash
# pq → ndjson → another pq
pq /tmp/events.parquet '.country, .revenue | to_json' \
  | pq -i ndjson - 'group_by .country | sum .revenue | top 3 by sum_revenue'
```

Why not write it as a single query? Because once stages multiply, pulling
them into separate steps lets you inspect intermediate results — that's
**a debugging-experience win, not a performance optimisation**.

### 3.4 Output parquet (preserve nested types for downstream)

```bash
# Write the query result as a new parquet file
pq /tmp/events.parquet '
  where .country == "US"
  | .user_id, .revenue, .events
' -o parquet > /tmp/us_only.parquet

ls -lh /tmp/us_only.parquet
pq schema /tmp/us_only.parquet
```

`-o parquet` streams to stdout via DuckDB's `COPY ... TO STDOUT (FORMAT PARQUET)`,
preserving nested types perfectly.

### 3.5 Quick guide: which output format?

| Use case | Format | Why |
|---|---|---|
| Pipe to jq / awk | `-o ndjson` or `to_json` stage | line-oriented |
| Open in Excel / Google Sheets | `-o csv` | with header, universal |
| Feed back into pq | `-o parquet` or `to_json` + `-i ndjson` | the former preserves schema, the latter is self-describing |
| Just look at it | `-o auto` (default) | table on TTY |

### Lesson 3 checkpoint

```bash
# Your turn: extract all users with at least one buy event into a new file
pq /tmp/events.parquet '
  where .events[].kind == "buy"
  | .user_id, .country, .revenue
' -o parquet > /tmp/buyers.parquet

pq schema /tmp/buyers.parquet
pq count /tmp/buyers.parquet
```

Got the new file? On to Lesson 4.

---

## Lesson 4: TUI in practice (5 min)

> **Key idea**: the TUI is for *exploring* an unfamiliar parquet and
> *iterating* on a query; once a query is finalised, the CLI form is what
> goes into your script.

### 4.1 Launch the TUI

```bash
pq tui /tmp/events.parquet
```

You'll see five panes:

```
┌──Columns──┐  ┌─────────Query─────────┐  ┌───Filters───┐
│ ▶ user_id │  │ .events[].kind        │  │ (none)      │
│   country │  │                        │  └─────────────┘
│   revenue │  └────────────────────────┘
│   ts      │  ┌──────────Data──────────┐
│   events  │  │ events_kind            │
│   metadata│  │ click                  │
└───────────┘  │ view                   │
                │ ...                    │
                └────────────────────────┘
```

### 4.2 The five keys you really need

| Key | Action |
|---|---|
| `Tab` | switch panes (Columns ↔ Query ↔ Data) |
| `Space` (in Columns) | toggle the current column into the projection |
| `Enter` (in Data) | drill down — auto-append a `where` clause built from this row's group-by values |
| `e` / `E` | toggle Explain pane (lowercase = estimates; uppercase = ANALYZE on real data) |
| `Q` | quit + **print the equivalent CLI to stdout**, ready to paste into a script |

### 4.3 Real walkthrough: from blank query to "country with most buys"

1. Launch: `pq tui /tmp/events.parquet`
2. `Tab` to the Query pane, clear the ghost text, type:
   ```
   .events[].kind
   ```
   Data pane lights up — every keystroke re-runs the query in <100 ms.
3. Change to `group_by .events[].kind | count`, sort descending. You'll
   see `click > view > buy > refund`.
4. Want the country breakdown too?
   ```
   group_by .country, .events[].kind | count
   ```
5. `Tab` to the Data pane, move the cursor to the `country=US, kind=buy`
   row, press `Enter` — Query pane updates to:
   ```
   group_by .country, .events[].kind | count where .country == "US" AND .events[].kind == "buy"
   ```
   That's the drill-down.
6. Press `e` for the Explain pane and read the heuristic hints
   ("💡 partition pushdown active" / "💡 try selecting fewer columns" etc.).
7. Happy with it? `Q`. stdout prints:
   ```
   pq /tmp/events.parquet 'group_by .country, .events[].kind | count where .country == "US" AND .events[].kind == "buy"'
   ```
   Paste into your script. Done.

### 4.4 When to use TUI vs CLI

| Situation | Use |
|---|---|
| First look at an unfamiliar parquet, schema not memorised | TUI |
| Query is finalised, going into cron / Airflow / Makefile | CLI |
| Debugging a query that's misbehaving | CLI + `--explain` |
| Demoing or teaching | TUI (live query feedback is great visually) |
| Big slow file, want to know "why is this slow" | TUI's Explain (`e`) + EXPLAIN ANALYZE (`E`) |

### Lesson 4 checkpoint

Launch the TUI, `Tab` between all three panes, write a `group_by`
query, drill down on a Data row by pressing `Enter`, and finally press
`Q` and copy the CLI form that gets printed.

Done? Move on to Lesson 5 (specifically about 30 GB+ files), or jump
straight to the cheat sheet.

---

## Lesson 5: Big-file mode (5 min — v0.12 + v0.13)

> **Key idea**: v0.12 / v0.13 turn pq into a big-file-friendly tool —
> streaming output, Ctrl-C interrupt, metadata-only `count --lite` /
> `stats --lite`, async TUI preview, and a stderr spinner.
> 30 GB+ files (local or cloud) feel as snappy as 30 KB ones.

### 5.1 Build a "big" file

The tutorial dataset is 30K — too small to feel any of this. Cook a
~200 MB one:

```bash
duckdb -c "
COPY (
  SELECT id AS user_id,
         ['US','UK','DE','FR','JP','CN'][((id-1) % 6) + 1] AS country,
         round((random() * 1000)::DOUBLE, 2) AS revenue,
         (random() * 86400)::BIGINT AS ts
  FROM range(1, 5000001) t(id)
) TO '/tmp/big.parquet' (FORMAT PARQUET);"

ls -lh /tmp/big.parquet
# -rw-r--r--  1 you  staff   ~200M
```

5 million rows, big enough to make the difference between "scan all"
and "metadata only" measurable.

### 5.2 Streaming output vs full aggregation (v0.12)

```bash
# ndjson / csv / raw-line outputs are now truly streaming —
# every row goes straight to stdout, never buffered. `head -1`
# returns instantly even on a 40 GB file.
pq -o ndjson /tmp/big.parquet '.user_id, .revenue' | head -1
# {"user_id":1,"revenue":342.18}

# Aggregations (group_by / count / sum etc.) still need a full
# scan because DuckDB has to hash everything before answering —
# that's a property of SQL, not pq.
pq /tmp/big.parquet 'group_by .country | sum .revenue'
```

Rule of thumb: **streaming = projection / filter / head**;
**full scan = aggregate / sort by**.

### 5.3 Metadata-only count (v0.12)

```bash
# pq count auto-enables lite mode for local parquet ≥ 1 GB —
# reads num_rows from the footer, no data scan. --lite forces it.
pq count --lite /tmp/big.parquet
# pq: lite mode (file >= 1.0 GB; reading row count from parquet footer)
# {"rows":5000000}

# Compare to a full count (scans everything) — orders of magnitude slower
time pq count /tmp/big.parquet
time pq count --lite /tmp/big.parquet
```

Tweak the threshold: `PQ_LITE_THRESHOLD=104857600 pq count file.parquet`
treats anything ≥ 100 MB as "big".

### 5.4 Metadata-only stats (v0.13)

```bash
# Full stats runs SUMMARIZE — exact, but reads every byte
pq stats /tmp/big.parquet
# {"column_name":"user_id","column_type":"BIGINT","min":"1","max":"5000000",
#  "approx_distinct":4998123,"null_pct":"0.00"}

# --lite reads each row group's footer stats — sub-second
pq stats --lite /tmp/big.parquet
# {"column_name":"user_id","column_type":"INT64","min":"1","max":"5000000",
#  "rows":5000000,"nulls":0}
```

Trade-off: lite has no `approx_distinct` / `null_pct` — those need
the data. Use full stats when you need the distinct count; use
`--lite` for a quick min/max/null/row glance.

### 5.5 Ctrl-C cancel (v0.12, CLI)

```bash
# Kick off a slow query, then SIGINT it 0.5s in
pq /tmp/big.parquet 'group_by .country | sum .revenue' &
sleep 0.5; kill -INT %1
# pq: interrupt requested (press Ctrl-C again to force-exit)
# Error: INTERRUPT
```

First Ctrl-C forwards SIGINT to DuckDB's `interrupt_handle.interrupt()`,
which unwinds cleanly from inside the parquet scan. Second Ctrl-C
falls through to a hard exit (130) — fallback for cases where
`interrupt()` blocks on a slow remote HTTP read.

### 5.6 Stderr spinner (v0.13)

```bash
# When stderr is a TTY, queries longer than 300 ms get a faint
# spinner on stderr. stdout stays clean.
pq /tmp/big.parquet 'group_by .country | count'
# stderr: ⠋  1.2s elapsed — Ctrl-C to cancel
# stdout: {"country":"CN","count":833334} ...

# Don't want it: --no-progress or PQ_NO_PROGRESS=1
pq /tmp/big.parquet '...' --no-progress
PQ_NO_PROGRESS=1 pq /tmp/big.parquet '...'

# In a pipeline it's auto-suppressed (stderr isn't a TTY) —
# CI logs and `2>` redirects stay clean
pq /tmp/big.parquet 'group_by .country | count' 2>err.log | jq .
cat err.log    # empty
```

### 5.7 TUI async preview (v0.13)

```bash
pq tui /tmp/big.parquet
```

Once inside the TUI, type any query. The Query pane header now shows:

```
┌─ Query · running 1.2s · Esc/Ctrl-C cancels ─┐
│ group_by .country, .revenue | top 10 by ... │
└──────────────────────────────────────────────┘
```

Pre-v0.13 every keystroke would freeze the event loop. Now the
preview runs on a worker thread — Esc / Ctrl-C interrupts the
in-flight preview, a second press quits the TUI.

### 5.8 Cloud big files (gs:// / s3://)

```bash
# Credentials auto-injected from AWS_* / PQ_GCS_* — no SET dance
export AWS_PROFILE=prod
pq count --lite s3://your-bucket/huge.parquet

# GCS via OAuth bearer token
export PQ_GCS_BEARER_TOKEN=$(gcloud auth print-access-token)
pq stats --lite gs://your-bucket/huge.parquet
```

Cloud paths **don't auto-enable lite mode** (probing the size needs
a network round-trip — too costly), so pass `--lite` explicitly.
HTTP request timeout drops to 15 s in v0.13 (default 30 s) so a
stuck remote request unblocks faster after a Ctrl-C; if your link
is flaky, override with `PQ_HTTP_TIMEOUT=60000`.

### Lesson 5 checkpoint

```bash
# Compare three count strategies on the 200 MB file
PQ_LITE_THRESHOLD=104857600 time pq count /tmp/big.parquet      # auto-lite
time pq count --lite /tmp/big.parquet                            # forced lite
time pq count /tmp/big.parquet                                   # full scan

# Verify lite stats includes the new rows + nulls columns
pq stats --lite /tmp/big.parquet

# Verify the spinner self-suppresses in a pipeline
pq /tmp/big.parquet 'group_by .country | count' 2>err.log | jq .
test ! -s err.log && echo "stderr was empty — spinner correctly silent"
```

Done? You've finished the main tutorial. Cheat sheet below for ongoing reference.

---

## Cheat sheet: common idioms

```bash
# === Exploration ===
pq schema FILE                                         # column names + types
pq stats FILE                                          # min/max/null%/distinct
pq count FILE                                          # row count
pq sample FILE -n 100                                  # random 100 rows
pq tui FILE                                            # enter the TUI

# === Project + filter ===
pq FILE '.col1, .col2 where .x == "y"'                 # inline form
pq FILE 'where .x == "y" | .col1, .col2'               # pipeline form
pq FILE '.* where .x is not null | head 50'            # all columns + filter

# === Group + aggregate ===
pq FILE 'group_by .country | count'                    # count
pq FILE 'group_by .country | sum .revenue | top 5 by sum_revenue'
pq FILE 'group_by .country | count, avg .revenue, max .revenue'

# === Nested ===
pq FILE '.events[0].kind'                              # first event's kind
pq FILE '.events[].kind'                               # row-explode
pq FILE 'group_by .events[].kind | count'              # group by event kind
pq FILE '.metadata["plan"]'                            # MAP lookup
pq FILE 'len(.events)'                                 # array length

# === Chains ===
pq FILE '.col | to_json' | jq ... | pq -i ndjson - '...'
cat FILE | pq -                                        # parquet over stdin
pq FILE '...' -o parquet > out.parquet                 # write parquet

# === Big files (v0.12 + v0.13) ===
pq count --lite FILE                                   # read footer, no scan
pq stats --lite FILE                                   # column min/max/nulls, no scan
pq -o ndjson FILE '...' | head -1                      # streaming output, instant
pq FILE '...' --no-progress                            # silence stderr spinner
PQ_LITE_THRESHOLD=104857600 pq count FILE              # 100 MB auto-lite trigger
PQ_HTTP_TIMEOUT=60000 pq count gs://...                # 60 s remote timeout

# === Debugging ===
pq FILE 'QUERY' --explain                              # see compiled SQL
pq FILE 'PRAGMA database_size'                         # raw SQL passthrough
```

---

## Next steps

- **For the full feature catalogue** (every stage's compilation rules,
  every key binding, every env var, every output format): see
  [`reference.md`](./reference.md).
- **For querying real big files / cloud storage**: read reference
  [§5 Data source resolution](./reference.md#5-data-source-resolution) and
  [§6 Cloud credentials](./reference.md#6-cloud-credentials-auto-injection),
  noting the stdin auto-spool caveats for files in the 40 GB range.
- **For version history / roadmap**: reference
  [§11](./reference.md#11-version-history) and
  [§12](./reference.md#12-roadmap).
- **Found a bug or want a feature?** Open an issue on the GitHub repo.

That's the tutorial. Happy querying.
