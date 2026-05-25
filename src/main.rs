// pq — jq for Parquet
//
// Usage examples:
//   pq file.parquet                                  # head 20
//   pq file.parquet '.user.id'                       # extract column
//   pq file.parquet 'country == "US"'                # filter rows
//   pq file.parquet '.email where .country == "US"'  # both (v0 inline)
//
//   # Pipe-stage syntax (v0.2):
//   pq f.parquet 'group_by .country | count | top 10 by count'
//   pq f.parquet 'where .age > 18 | group_by .country | avg .revenue'
//   pq f.parquet '.country | distinct'
//   pq f.parquet '.country, .revenue | sort by .revenue desc | limit 5'
//
//   # Globs auto-expand via DuckDB:
//   pq 'data/dt=2026-*/*.parquet' 'group_by .dt | count'
//
//   # Subcommands:
//   pq schema | stats | count | sample | head | tail   <file>
//
//   # Export back to parquet (full file, no LIMIT):
//   pq big.parquet '.country == "US"' -o parquet > us.parquet
//
// Backends: DuckDB (embedded). Reads local paths, globs, gs://, s3://, az://.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use duckdb::Connection;

mod cloud;
mod lineage;
mod output;
mod parser;
mod progress;
mod source;
mod tui;

use crate::output::OutputFormat;
use crate::parser::compile_plan_fmt;
use crate::source::{InputFormat, StdinSpool};

#[derive(Parser, Debug)]
#[command(
    name = "pq",
    version,
    about = "jq for Parquet",
    long_about = None,
    // If user supplies positional args (file/query), don't try to interpret
    // the second positional as a subcommand. So `pq f.parquet count` parses
    // as (file=f.parquet, query=count) instead of routing to Cmd::Count.
    args_conflicts_with_subcommands = true,
)]
struct Cli {
    /// Input parquet (local path, glob, gs://, s3://, az://) or "-" for stdin
    file: Option<String>,

    /// Query expression. Stages are separated by `|`.
    /// Supported verbs: select, where, group_by, count, sum/avg/min/max,
    /// count_distinct, top N by, sort by, limit, head, distinct.
    query: Option<String>,

    /// Output: auto | table | json | ndjson | csv | parquet
    #[arg(short, long, default_value = "auto", global = true)]
    output: String,

    /// Input format: auto | parquet | ndjson (jsonl/json) | csv (tsv).
    /// `auto` sniffs from file extension (.ndjson/.csv → matching reader,
    /// otherwise parquet). For stdin (`-`) auto means parquet — pass an
    /// explicit `-i ndjson` to chain `pq f.parquet '...' | pq -i ndjson '...'`.
    #[arg(short = 'i', long, default_value = "auto", global = true)]
    input: String,

    /// Row limit for default head; default 20. Use 0 for no limit.
    /// (Auto-disabled when -o parquet, so full exports work as expected.)
    #[arg(short = 'n', long, default_value_t = 20)]
    n: usize,

    /// Show the SQL pq would run, but don't execute (for debugging the parser)
    #[arg(long, global = true)]
    explain: bool,

    /// Watch mode: re-run the query every N seconds (clearing the screen between
    /// runs). Useful for live dashboards on directories that get new files.
    #[arg(short = 'w', long, value_name = "SECS")]
    watch: Option<u64>,

    /// Register a DuckDB SQL macro before running the query. Repeatable.
    /// Format: `name(args) := body` (the `:=` is rewritten to SQL `AS`).
    /// Example: `--udf 'is_us(c) := c = ''US'''`
    #[arg(long = "udf", value_name = "MACRO")]
    udfs: Vec<String>,

    /// Suppress the stderr progress spinner / elapsed timer that
    /// shows up on long-running queries. v0.13 — by default pq
    /// draws a `⠋ 1.2s elapsed — Ctrl-C to cancel` line on stderr
    /// when stderr is a TTY and the query has been running for
    /// more than 300 ms; this flag (and `PQ_NO_PROGRESS=1`) turn
    /// it off for scripts that capture stderr.
    #[arg(long = "no-progress", global = true)]
    no_progress: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

// ── Subcommand-local flags ───────────────────────────────────────────────────
//
// Why every subcommand re-declares `-i input`: clap's
// `args_conflicts_with_subcommands` (set on `Cli`) makes parent-level flags
// disable subcommand routing — without that flag `pq f.parquet 'count'`
// would route to `Cmd::Count` and bomb on a missing file. We need the
// flag to keep that idiom working. The price is that parent flags can't
// be combined with a subcommand at all (clap silently drops to positional
// parsing instead, which is how `pq -i ndjson schema FILE` used to mis-
// parse `schema` as the file). Putting `-i` directly on each subcommand
// is the standard escape hatch — same pattern kubectl/docker subcommands
// use. Flags-after-subcommand-name reads naturally; the user's tab-
// completion learns it instantly.

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show parquet schema as a table.
    Schema {
        file: String,
        /// Input format (auto | parquet | ndjson | csv).
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
    },
    /// Per-column min / max / null / distinct stats.
    Stats {
        file: String,
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
        /// Force the metadata-only path (parquet_metadata).
        ///
        /// On parquet inputs this reads per-row-group min/max/null
        /// stats from each file's footer and aggregates them — no
        /// scan, no decompress. Default `pq stats` runs `SUMMARIZE`,
        /// which is exact but reads every byte; on a multi-GB file
        /// the lite path is the difference between sub-second and
        /// minutes. Lite output omits `approx_distinct` and
        /// `null_pct` (those need the data); ships row counts and
        /// min/max only. Auto-on for local parquet >= 1 GB; this
        /// flag forces it elsewhere (e.g. cloud paths). Override the
        /// auto threshold with PQ_LITE_THRESHOLD.
        #[arg(long = "lite")]
        lite: bool,
    },
    /// Random sample of N rows.
    Sample {
        file: String,
        #[arg(short, long, default_value_t = 10)]
        n: usize,
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
    },
    /// First N rows.
    Head {
        file: String,
        #[arg(short, long, default_value_t = 20)]
        n: usize,
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
    },
    /// Last N rows.
    Tail {
        file: String,
        #[arg(short, long, default_value_t = 20)]
        n: usize,
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
    },
    /// Total row count.
    Count {
        file: String,
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
        /// Force the metadata-only path (parquet_file_metadata).
        ///
        /// On parquet inputs this skips the data scan entirely and
        /// reads the row count from the footer of every matched file —
        /// orders of magnitude faster on multi-GB files. Auto-on for
        /// local parquet files >= 1 GB; this flag forces it on
        /// otherwise (e.g. cloud paths where size detection is hard).
        #[arg(long = "lite")]
        lite: bool,
    },
    /// Interactive TUI: 4-panel browser (Columns / Filters / editable Query / live Data).
    /// On exit, prints the equivalent `pq` CLI one-liner so you can copy it
    /// into a shell history, Makefile, or cron job. (v0.5 MVP)
    Tui {
        file: String,
        /// Input format for the file (auto | parquet | ndjson | csv).
        #[arg(short = 'i', long, default_value = "auto")]
        input: String,
        /// Cap on rows shown in the Data panel. Default 50; honored when set.
        #[arg(short = 'n', long, default_value_t = 50)]
        n: usize,
        /// SQL macro to register before the session. Repeatable.
        /// Format: `name(args) := body` (the `:=` is rewritten to SQL `AS`).
        #[arg(long = "udf", value_name = "MACRO")]
        udfs: Vec<String>,
    },
}

fn main() -> Result<()> {
    // v0.12.1: restore the conventional Unix SIGPIPE behaviour. Rust's
    // runtime sets SIGPIPE to SIG_IGN at startup, which means writing
    // to a closed pipe surfaces as `Err(EPIPE)` instead of silently
    // terminating the process. Once we started streaming output (so
    // `pq -o ndjson f.parquet | head -1` only consumes one row), every
    // such invocation produced "Error: Broken pipe (os error 32)" on
    // stderr and exited non-zero. Resetting to SIG_DFL matches the
    // behaviour of every other Unix CLI tool (cat, grep, sed, jq).
    reset_sigpipe();

    let cli = Cli::parse();
    let conn = open_conn()?;
    register_udfs(&conn, &cli.udfs)?;

    // v0.12: SIGINT translates into DuckDB's interrupt() instead of an
    // immediate kill. Long parquet scans get a chance to unwind cleanly
    // and surface a "Query interrupted" error; second Ctrl-C exits hard
    // in case the first interrupt itself takes too long (e.g. blocked
    // on a network read against gs:// or s3://).
    install_sigint_handler(&conn);

    let fmt = OutputFormat::resolve(&cli.output);

    // Watch mode wraps execution in a loop with an ANSI screen-clear before each tick.
    if let Some(secs) = cli.watch {
        if secs == 0 {
            return Err(anyhow!("--watch interval must be > 0"));
        }
        let start = std::time::Instant::now();
        let mut tick: u64 = 0;
        loop {
            tick += 1;
            // \x1b[2J clears the screen, \x1b[H homes the cursor.
            print!("\x1b[2J\x1b[H");
            eprintln!(
                "── pq watch — tick #{tick}, {:.0}s elapsed, every {secs}s — Ctrl-C to stop ──",
                start.elapsed().as_secs_f64()
            );
            if let Err(e) = run_query(&cli, &conn, fmt) {
                eprintln!("error: {e:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }

    run_query(&cli, &conn, fmt)
}

fn run_query(cli: &Cli, conn: &Connection, fmt: OutputFormat) -> Result<()> {
    if let Some(cmd) = cli.command.as_ref() {
        return run_subcommand(conn, cmd, fmt, cli);
    }

    let file = cli
        .file
        .as_ref()
        .ok_or_else(|| anyhow!("a parquet file is required (try: pq <file>)"))?;
    let query = cli.query.as_deref().unwrap_or("");

    // Resolve the input format: explicit --input wins, else sniff from
    // the file extension. Stdin (`-`) defaults to parquet — that's the
    // historical behaviour and the most common shape (`pq - < f.parquet`).
    let input_fmt = resolve_input_format(&cli.input, file)?;

    // v0.9: if the user piped a parquet file into us (`cat f.parquet | pq -`)
    // the fd is a non-seekable pipe, which DuckDB rejects. StdinSpool drains
    // stdin into a tempfile when needed; the spool guard lives until the
    // end of run_query so the file is still on disk while we read it.
    let spool = StdinSpool::resolve(file, input_fmt)?;

    // For parquet export, the user almost never wants the default head LIMIT.
    let effective_n = if fmt == OutputFormat::Parquet {
        0
    } else {
        cli.n
    };
    let plan = compile_plan_fmt(&spool.resolved, query, effective_n, input_fmt)?;

    if cli.explain {
        println!("{}", plan.sql);
        return Ok(());
    }

    // `to_csv` / `to_json` stages override the user's `-o` choice — the whole
    // point of those stages is "I want raw lines on stdout".
    let effective_fmt = if plan.raw_lines {
        OutputFormat::RawLines
    } else {
        fmt
    };
    // v0.13: stderr spinner for long-running queries. RAII drop
    // clears the line as soon as run_and_print returns. No-op when
    // stderr isn't a TTY, --no-progress is set, or PQ_NO_PROGRESS=1.
    let _spinner = progress::Spinner::maybe_start(cli.no_progress);
    output::run_and_print(conn, &plan.sql, effective_fmt)
}

/// Register user-supplied SQL macros on the connection before any queries run.
///
/// Accepted forms (per --udf flag):
///   `name(args) := body`         — pq sugar; we rewrite `:=` to ` AS `
///   `name(args) AS body`         — DuckDB native; passed through verbatim
///   anything else                — wrapped as `CREATE MACRO <input>` and
///                                  whatever DuckDB makes of it is the error.
pub(crate) fn register_udfs(conn: &Connection, udfs: &[String]) -> Result<()> {
    for raw in udfs {
        let body = raw.trim();
        // Tolerate users wrapping their macro body in `CREATE MACRO ...` or not.
        let normalized = if body.to_ascii_lowercase().starts_with("create ") {
            body.to_string()
        } else if let Some(idx) = body.find(":=") {
            format!(
                "CREATE OR REPLACE MACRO {} AS {}",
                body[..idx].trim(),
                body[idx + 2..].trim()
            )
        } else {
            format!("CREATE OR REPLACE MACRO {body}")
        };
        conn.execute_batch(&normalized)
            .with_context(|| format!("failed to register --udf: {raw}"))?;
    }
    Ok(())
}

/// Decide the input format from the `--input` flag plus a (possibly stdin)
/// file argument. `auto` + a real path sniffs the extension; `auto` + `-`
/// defaults to parquet (matches the historical behaviour and the most
/// common chain shape `pq - < f.parquet`).
fn resolve_input_format(flag: &str, file: &str) -> Result<InputFormat> {
    if let Some(explicit) = InputFormat::from_flag(flag)? {
        return Ok(explicit);
    }
    if file == "-" {
        return Ok(InputFormat::Parquet);
    }
    Ok(InputFormat::from_extension(file))
}

pub(crate) fn open_conn() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("failed to open DuckDB connection")?;
    // Enable cloud httpfs for gs:// / s3:// — duckdb's httpfs is bundled with our build.
    // We swallow errors here because httpfs may already be loaded on some builds.
    let _ = conn.execute_batch(
        r"
        INSTALL httpfs;
        LOAD httpfs;
        ",
    );
    // v0.13: shrink the HTTPFS request timeout so a stuck remote
    // request unblocks within 15 s of an interrupt. Default is 30 s
    // which feels indistinguishable from "TUI hung" when a user
    // presses Ctrl-C against a slow gs:// / s3:// scan. Setting is
    // best-effort — if the duckdb build is too old to know the
    // pragma we keep the default. http_keep_alive=true (the default)
    // is set explicitly so we don't tear down TCP between row-group
    // requests on the same parquet file. Override with PQ_HTTP_TIMEOUT
    // in milliseconds (e.g. PQ_HTTP_TIMEOUT=60000 for very flaky links).
    let timeout_ms = std::env::var("PQ_HTTP_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15000);
    let _ = conn.execute_batch(&format!(
        "SET http_keep_alive=true; SET http_timeout={timeout_ms};"
    ));
    // Auto-inject any cloud creds visible in the env (PQ_GCS_HMAC_*, AWS_*).
    // Failures are silent unless PQ_DEBUG=1 — we never block local-file usage
    // because someone has stale env vars sitting around.
    cloud::inject_credentials(&conn);
    Ok(conn)
}

/// Reset SIGPIPE to its default OS-level disposition (terminate the
/// process). Rust's runtime sets it to SIG_IGN at startup as a safety
/// measure, but for a CLI tool that streams output through pipes
/// (`pq … | head`, `pq … | jq`), the conventional Unix behaviour is
/// preferred: when the downstream reader closes its end of the pipe,
/// the kernel sends SIGPIPE and we exit silently. Without this reset
/// we'd surface every closed-pipe write as a noisy `Err` to the user
/// even though they got exactly the output they asked for.
#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY: setting a signal handler to SIG_DFL is documented as
    // safe in POSIX; libc::signal returns the previous handler which
    // we ignore. No other thread can be running yet (we're in main
    // before any tokio/std::thread spawns), so no signal-handler race.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {
    // Windows has no SIGPIPE; broken-pipe writes already surface as
    // a normal Err which the caller can handle.
}

/// Wire SIGINT to DuckDB's interrupt API so Ctrl-C cancels the running
/// query instead of killing pq mid-stream.
///
/// Behaviour:
///   * 1st Ctrl-C → call `InterruptHandle::interrupt()` and write a
///     short notice to stderr. DuckDB returns from `query()` with an
///     "INTERRUPT" error which surfaces as a normal `Err(_)` we print
///     via the existing error path.
///   * 2nd Ctrl-C → exit immediately with code 130 (128 + SIGINT). This
///     is the safety net for cases where interrupt() can't unwind fast
///     enough (very long network reads against gs://, s3://, etc.).
fn install_sigint_handler(conn: &Connection) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ALREADY_INTERRUPTED: AtomicBool = AtomicBool::new(false);

    let handle = conn.interrupt_handle();
    if let Err(e) = ctrlc::set_handler(move || {
        if ALREADY_INTERRUPTED.swap(true, Ordering::SeqCst) {
            // Second hit — bail out hard.
            std::process::exit(130);
        }
        eprintln!("\npq: interrupt requested (press Ctrl-C again to force-exit)");
        handle.interrupt();
    }) {
        // Don't fail the run if we can't install the handler — fall
        // back to the OS default (immediate kill on SIGINT). This is
        // possible in oddball environments (already-installed handler,
        // signal disposition pinned by parent process, etc.).
        if std::env::var_os("PQ_DEBUG").is_some() {
            eprintln!("pq: failed to install SIGINT handler: {e}");
        }
    }
}

fn run_subcommand(conn: &Connection, cmd: &Cmd, fmt: OutputFormat, cli: &Cli) -> Result<()> {
    // The TUI takes over the terminal — handle it before we build any SQL.
    if let Cmd::Tui {
        file,
        input,
        n,
        udfs,
    } = cmd
    {
        // Hand the TUI its own connection so it owns the duckdb session.
        // Cheap (in-memory db, ~50 ms) and cleaner than threading a borrow
        // through ratatui's event-loop closures. Re-register UDFs on the
        // fresh connection so `--udf … tui f.parquet` works (without this
        // the macros lived only on the parent `conn`).
        let tui_conn = open_conn()?;
        register_udfs(&tui_conn, udfs)?;
        let _ = conn; // explicit acknowledgement that we don't reuse `conn`
        let input_fmt = resolve_input_format(input, file)?;
        // `-n` is the user's preview-row cap. Subcommand-level default is
        // 50 (TUI's historical preview height); we floor at 1 to avoid an
        // empty viewport.
        let preview_limit = (*n).max(1);
        return tui::run(
            tui_conn,
            file.clone(),
            input_fmt,
            preview_limit,
            udfs.clone(),
        );
    }

    // v0.9: subcommands also get stdin-spool + format dispatch. Each
    // variant carries its own `-i input`, so the resolution lives inline
    // here and we don't reach into the parent `Cli` for flags (which
    // can't be combined with a subcommand under our clap config anyway).
    let (raw_file, input_str) = match cmd {
        Cmd::Schema { file, input }
        | Cmd::Stats { file, input, .. }
        | Cmd::Sample { file, input, .. }
        | Cmd::Head { file, input, .. }
        | Cmd::Tail { file, input, .. }
        | Cmd::Count { file, input, .. } => (file.clone(), input.as_str()),
        Cmd::Tui { .. } => unreachable!("Tui handled before this match"),
    };
    let input_fmt = resolve_input_format(input_str, &raw_file)?;
    let spool = StdinSpool::resolve(&raw_file, input_fmt)?;
    let src = parser::source_clause_fmt(&spool.resolved, input_fmt);

    let sql = match cmd {
        Cmd::Schema { .. } => format!(
            "SELECT column_name, column_type, \"null\" AS nullable \
             FROM (DESCRIBE SELECT * FROM {src})"
        ),
        Cmd::Stats { lite, .. } => stats_sql(&spool.resolved, input_fmt, *lite, &src),
        Cmd::Sample { n, .. } => {
            format!("SELECT * FROM {src} USING SAMPLE {n} ROWS")
        }
        Cmd::Head { n, .. } => format!("SELECT * FROM {src} LIMIT {n}"),
        Cmd::Tail { n, .. } => format!(
            "WITH t AS (SELECT *, row_number() OVER () AS __rn FROM {src}) \
             SELECT * EXCLUDE (__rn) FROM t \
             ORDER BY __rn DESC LIMIT {n}"
        ),
        Cmd::Count { lite, .. } => count_sql(&spool.resolved, input_fmt, *lite, &src),
        Cmd::Tui { .. } => unreachable!("Tui handled before this match"),
    };

    let _spinner = progress::Spinner::maybe_start(cli.no_progress);
    output::run_and_print(conn, &sql, fmt)
}

/// Build the SQL for `pq count`.
///
/// Default path is a regular `count(*) FROM read_parquet(...)`. v0.12
/// adds a metadata-only fast path: when the input is parquet **and**
/// either the user passed `--lite` or the file is large enough to
/// trip the auto-lite threshold (1 GB by default, override via
/// `PQ_LITE_THRESHOLD` in bytes), we read row counts straight from
/// the parquet footer via `parquet_file_metadata(...)`. No scan, no
/// decompress — just the file's own self-reported row count. On a
/// 40 GB local file this is the difference between seconds and a
/// fraction of a second; on globs it's per-file row counts summed.
fn count_sql(resolved_path: &str, input_fmt: InputFormat, forced: bool, src: &str) -> String {
    let threshold = lite_threshold();
    let auto = should_auto_lite(resolved_path, threshold);
    if input_fmt == InputFormat::Parquet && (forced || auto) {
        let escaped = resolved_path.replace('\'', "''");
        if auto && !forced {
            // Inform on stderr so a non-TTY consumer of stdout (jq,
            // awk, > redirect) doesn't see the banner. Ignored if the
            // user redirected stderr too; that's their call.
            eprintln!(
                "pq: lite mode (file >= {}; reading row count from parquet footer)",
                fmt_bytes(threshold)
            );
        }
        format!("SELECT sum(num_rows)::BIGINT AS rows FROM parquet_file_metadata('{escaped}')")
    } else {
        format!("SELECT count(*) AS rows FROM {src}")
    }
}

/// Build the SQL for `pq stats`.
///
/// Default path runs DuckDB's `SUMMARIZE` over the data. v0.13 adds a
/// metadata-only path that aggregates `parquet_metadata(...)` per
/// row group: per column → `min(stats_min_value)`,
/// `max(stats_max_value)`, `sum(stats_null_count)`, total rows. No
/// scan, no decompress — works in sub-second on a 30 GB file.
///
/// Caveats of lite mode (documented in the README):
///   * No `approx_distinct` / `null_pct` — those need the data.
///   * `min` / `max` come from the writer's row-group stats. Most
///     parquet writers populate them, but a few (older Spark,
///     non-default Pandas) skip them for STRING columns; lite then
///     reports NULL for those bounds.
///   * One row per leaf field path. Nested STRUCT / LIST fields show
///     up by their dotted path, same as DuckDB's `parquet_metadata`
///     output. That's a feature for skinny parquet schemas (you see
///     stats for nested array elements) but it does mean rows don't
///     line up 1:1 with the top-level schema.
fn stats_sql(resolved_path: &str, input_fmt: InputFormat, forced: bool, src: &str) -> String {
    let threshold = lite_threshold();
    let auto = should_auto_lite(resolved_path, threshold);
    if input_fmt == InputFormat::Parquet && (forced || auto) {
        let escaped = resolved_path.replace('\'', "''");
        if auto && !forced {
            eprintln!(
                "pq: lite mode (file >= {}; reading column stats from parquet footer)",
                fmt_bytes(threshold)
            );
        }
        // any_value(type) is fine here — Parquet schema metadata is
        // consistent across row groups for the same column path.
        // num_values + stats_null_count gives us a total non-null
        // count without scanning.
        format!(
            "SELECT path_in_schema AS column_name, \
                    any_value(type) AS column_type, \
                    min(stats_min_value)::VARCHAR AS min, \
                    max(stats_max_value)::VARCHAR AS max, \
                    sum(num_values)::BIGINT AS rows, \
                    sum(stats_null_count)::BIGINT AS nulls \
             FROM parquet_metadata('{escaped}') \
             GROUP BY path_in_schema \
             ORDER BY min(row_group_id), min(column_id)"
        )
    } else {
        format!(
            "SELECT column_name, column_type, min, max, \
                    approx_unique AS approx_distinct, \
                    null_percentage AS null_pct \
             FROM (SUMMARIZE SELECT * FROM {src})"
        )
    }
}

/// Threshold in bytes above which we auto-enable lite mode for local
/// parquet files. Default 1 GB; override via `PQ_LITE_THRESHOLD` (raw
/// integer bytes, no suffix parsing — keep it simple).
fn lite_threshold() -> u64 {
    const DEFAULT: u64 = 1024 * 1024 * 1024; // 1 GB
    std::env::var("PQ_LITE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT)
}

/// True when `path` looks like a local file/glob whose size reaches
/// the auto-lite threshold. Cloud paths (`gs://`, `s3://`, `http://`)
/// always return false — checking size requires a network round-trip
/// we don't want to spend; the user can pass `--lite` explicitly.
///
/// For globs, we sum the sizes of every matched file. If glob
/// expansion fails (DuckDB-side glob, can't tell from here), we
/// conservatively return false — the slow path still works, just
/// without the lite shortcut.
fn should_auto_lite(path: &str, threshold: u64) -> bool {
    if path.contains("://") {
        return false;
    }
    if let Some(ix) = path.find(['*', '?', '[']) {
        // Glob: walk siblings of the directory before the first
        // wildcard. We cap at 64 entries so a `**` that matches
        // everything doesn't turn into a `du -s` proxy.
        let dir = std::path::Path::new(&path[..ix])
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut total: u64 = 0;
        for (count, entry) in entries.flatten().enumerate() {
            if count >= 64 {
                break;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                    if total >= threshold {
                        return true;
                    }
                }
            }
        }
        false
    } else {
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.len() >= threshold)
            .unwrap_or(false)
    }
}

fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v.fract() < 0.05 {
        format!("{:.0} {}", v, UNITS[i])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;

    #[test]
    fn fmt_bytes_picks_right_unit() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1 KB");
        assert_eq!(fmt_bytes(1500), "1.5 KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1 MB");
        assert_eq!(fmt_bytes(1024 * 1024 * 1024), "1 GB");
        assert_eq!(fmt_bytes(1024u64 * 1024 * 1024 * 1024), "1 TB");
    }

    #[test]
    #[serial]
    fn lite_threshold_default_is_one_gb() {
        // PQ_LITE_THRESHOLD must NOT be set during this test for the
        // default to apply. Tests in this module run in-process so an
        // env var set by a sibling test could leak through; clear
        // defensively to keep this test order-independent.
        // SAFETY: env access is process-global; we serialize this test
        // to prevent races with sibling tests that mutate the env.
        unsafe {
            std::env::remove_var("PQ_LITE_THRESHOLD");
        }
        assert_eq!(lite_threshold(), 1024 * 1024 * 1024);
    }

    #[test]
    #[serial]
    fn lite_threshold_honors_override() {
        // SAFETY: see lite_threshold_default_is_one_gb.
        unsafe {
            std::env::set_var("PQ_LITE_THRESHOLD", "2048");
        }
        assert_eq!(lite_threshold(), 2048);
        unsafe {
            std::env::remove_var("PQ_LITE_THRESHOLD");
        }
    }

    #[test]
    fn auto_lite_skips_remote_paths() {
        // Cloud paths always return false — checking sizes over the
        // network would be a footgun on `pq schema gs://huge.parquet`.
        let threshold = 1; // arbitrary; remote paths always skip
        assert!(!should_auto_lite("gs://bucket/big.parquet", threshold));
        assert!(!should_auto_lite("s3://b/x.parquet", threshold));
        assert!(!should_auto_lite(
            "https://example.com/x.parquet",
            threshold
        ));
    }

    #[test]
    fn auto_lite_below_threshold_returns_false() {
        // A small temp file shouldn't trip a large threshold.
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        f.write_all(b"hello").expect("write");
        let path = f.path().to_string_lossy().into_owned();
        // Pass a large threshold so the 5-byte file stays below it.
        let large_threshold = 1024 * 1024 * 1024; // 1 GB
        assert!(!should_auto_lite(&path, large_threshold));
    }

    #[test]
    fn auto_lite_above_threshold_returns_true() {
        // Pass a threshold of 1 byte so any non-empty file qualifies.
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        f.write_all(b"hello").expect("write");
        let path = f.path().to_string_lossy().into_owned();
        // No env var mutations needed — pass threshold directly.
        assert!(should_auto_lite(&path, 1));
    }

    #[test]
    fn count_sql_lite_uses_metadata_function_for_parquet() {
        // Forced lite + parquet → metadata-only SQL, no banner since
        // forced means user opted in.
        let sql = count_sql(
            "/tmp/x.parquet",
            InputFormat::Parquet,
            true,
            "read_parquet('/tmp/x.parquet')",
        );
        assert!(
            sql.contains("parquet_file_metadata"),
            "lite forced should pick metadata path; got: {sql}"
        );
    }

    #[test]
    fn count_sql_falls_back_to_count_star_for_ndjson_even_when_lite() {
        // NDJSON has no parquet metadata function; lite mode is a
        // no-op for non-parquet inputs.
        let sql = count_sql(
            "/tmp/x.ndjson",
            InputFormat::Ndjson,
            true,
            "read_json('/tmp/x.ndjson', format='newline_delimited')",
        );
        assert!(
            sql.contains("count(*)"),
            "non-parquet lite should fall back to count(*); got: {sql}"
        );
    }

    #[test]
    fn count_sql_default_path_uses_count_star() {
        // No --lite, no auto-trigger → original count(*) path so
        // existing CLI behaviour is preserved.
        let sql = count_sql(
            "/nonexistent.parquet",
            InputFormat::Parquet,
            false,
            "read_parquet('/nonexistent.parquet')",
        );
        assert!(
            sql.contains("count(*)"),
            "default count path should be count(*); got: {sql}"
        );
    }

    #[test]
    fn stats_sql_lite_uses_parquet_metadata_for_parquet() {
        // Forced lite + parquet → metadata SQL aggregating row-group
        // stats, no scan. Banner is suppressed because the user opted
        // in; we only print it on auto-trigger.
        let sql = stats_sql(
            "/tmp/x.parquet",
            InputFormat::Parquet,
            true,
            "read_parquet('/tmp/x.parquet')",
        );
        assert!(
            sql.contains("parquet_metadata"),
            "lite forced should pick metadata path; got: {sql}"
        );
        assert!(
            !sql.contains("SUMMARIZE"),
            "lite forced should NOT use SUMMARIZE; got: {sql}"
        );
    }

    #[test]
    fn stats_sql_lite_orders_columns_by_schema_position() {
        // Schema order is preserved via min(row_group_id) +
        // min(column_id), not alphabetic. This was a real bug
        // during v0.13 dev — `file_offset` is not always populated,
        // so the first ordering attempt fell through to alphabetic
        // and a 50-column schema came out scrambled.
        let sql = stats_sql(
            "/tmp/x.parquet",
            InputFormat::Parquet,
            true,
            "read_parquet('/tmp/x.parquet')",
        );
        assert!(
            sql.contains("ORDER BY min(row_group_id), min(column_id)"),
            "lite SQL should preserve schema order; got: {sql}"
        );
    }

    #[test]
    fn stats_sql_falls_back_to_summarize_for_ndjson() {
        // Non-parquet inputs ignore --lite and run SUMMARIZE — there's
        // no metadata footer to read.
        let sql = stats_sql(
            "/tmp/x.ndjson",
            InputFormat::Ndjson,
            true,
            "read_json('/tmp/x.ndjson', format='newline_delimited')",
        );
        assert!(
            sql.contains("SUMMARIZE"),
            "non-parquet lite should fall back to SUMMARIZE; got: {sql}"
        );
    }

    #[test]
    fn stats_sql_default_path_uses_summarize() {
        // No --lite, no auto-trigger → original SUMMARIZE path so
        // existing CLI behaviour is preserved bit-for-bit.
        let sql = stats_sql(
            "/nonexistent.parquet",
            InputFormat::Parquet,
            false,
            "read_parquet('/nonexistent.parquet')",
        );
        assert!(
            sql.contains("SUMMARIZE"),
            "default stats path should be SUMMARIZE; got: {sql}"
        );
    }
}
