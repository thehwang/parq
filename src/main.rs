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
    #[arg(short, long, default_value = "auto")]
    output: String,

    /// Input format: auto | parquet | ndjson (jsonl/json) | csv (tsv).
    /// `auto` sniffs from file extension (.ndjson/.csv → matching reader,
    /// otherwise parquet). For stdin (`-`) auto means parquet — pass an
    /// explicit `-i ndjson` to chain `pq f.parquet '...' | pq -i ndjson '...'`.
    #[arg(short = 'i', long, default_value = "auto")]
    input: String,

    /// Row limit for default head; default 20. Use 0 for no limit.
    /// (Auto-disabled when -o parquet, so full exports work as expected.)
    #[arg(short = 'n', long, default_value_t = 20)]
    n: usize,

    /// Show the SQL pq would run, but don't execute (for debugging the parser)
    #[arg(long)]
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

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show parquet schema as a table.
    Schema { file: String },
    /// Per-column min / max / null / distinct stats.
    Stats { file: String },
    /// Random sample of N rows.
    Sample {
        file: String,
        #[arg(short, long, default_value_t = 10)]
        n: usize,
    },
    /// First N rows.
    Head {
        file: String,
        #[arg(short, long, default_value_t = 20)]
        n: usize,
    },
    /// Last N rows.
    Tail {
        file: String,
        #[arg(short, long, default_value_t = 20)]
        n: usize,
    },
    /// Total row count.
    Count { file: String },
    /// Interactive TUI: 4-panel browser (Columns / Filters / editable Query / live Data).
    /// On exit, prints the equivalent `pq` CLI one-liner so you can copy it
    /// into a shell history, Makefile, or cron job. (v0.5 MVP)
    Tui { file: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = open_conn()?;
    register_udfs(&conn, &cli.udfs)?;
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
    output::run_and_print(conn, &plan.sql, effective_fmt)
}

/// Register user-supplied SQL macros on the connection before any queries run.
///
/// Accepted forms (per --udf flag):
///   `name(args) := body`         — pq sugar; we rewrite `:=` to ` AS `
///   `name(args) AS body`         — DuckDB native; passed through verbatim
///   anything else                — wrapped as `CREATE MACRO <input>` and
///                                  whatever DuckDB makes of it is the error.
fn register_udfs(conn: &Connection, udfs: &[String]) -> Result<()> {
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
    // Auto-inject any cloud creds visible in the env (PQ_GCS_HMAC_*, AWS_*).
    // Failures are silent unless PQ_DEBUG=1 — we never block local-file usage
    // because someone has stale env vars sitting around.
    cloud::inject_credentials(&conn);
    Ok(conn)
}

fn run_subcommand(conn: &Connection, cmd: &Cmd, fmt: OutputFormat, cli: &Cli) -> Result<()> {
    // The TUI takes over the terminal — handle it before we build any SQL.
    if let Cmd::Tui { file } = cmd {
        // Hand the TUI its own connection so it owns the duckdb session.
        // Cheap (in-memory db, ~50 ms) and cleaner than threading a borrow
        // through ratatui's event-loop closures.
        let tui_conn = open_conn()?;
        let _ = conn; // explicit acknowledgement that we don't reuse `conn`
        return tui::run(tui_conn, file.clone());
    }

    // v0.9: subcommands also get stdin-spool + format dispatch. Pull out
    // the file from the matched variant, resolve, then build SQL against
    // the resolved path. The spool guard must outlive the read — keep it
    // bound for the whole match arm.
    let raw_file = match cmd {
        Cmd::Schema { file }
        | Cmd::Stats { file }
        | Cmd::Sample { file, .. }
        | Cmd::Head { file, .. }
        | Cmd::Tail { file, .. }
        | Cmd::Count { file } => file.clone(),
        Cmd::Tui { .. } => unreachable!("Tui handled before this match"),
    };
    let input_fmt = resolve_input_format(&cli.input, &raw_file)?;
    let spool = StdinSpool::resolve(&raw_file, input_fmt)?;
    let src = parser::source_clause_fmt(&spool.resolved, input_fmt);

    let sql = match cmd {
        Cmd::Schema { .. } => format!(
            "SELECT column_name, column_type, \"null\" AS nullable \
             FROM (DESCRIBE SELECT * FROM {src})"
        ),
        Cmd::Stats { .. } => format!(
            "SELECT column_name, column_type, min, max, \
                    approx_unique AS approx_distinct, \
                    null_percentage AS null_pct \
             FROM (SUMMARIZE SELECT * FROM {src})"
        ),
        Cmd::Sample { n, .. } => {
            format!("SELECT * FROM {src} USING SAMPLE {n} ROWS")
        }
        Cmd::Head { n, .. } => format!("SELECT * FROM {src} LIMIT {n}"),
        Cmd::Tail { n, .. } => format!(
            "WITH t AS (SELECT *, row_number() OVER () AS __rn FROM {src}) \
             SELECT * EXCLUDE (__rn) FROM t \
             ORDER BY __rn DESC LIMIT {n}"
        ),
        Cmd::Count { .. } => format!("SELECT count(*) AS rows FROM {src}"),
        Cmd::Tui { .. } => unreachable!("Tui handled before this match"),
    };

    output::run_and_print(conn, &sql, fmt)
}
