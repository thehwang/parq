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

mod output;
mod parser;

use crate::output::OutputFormat;
use crate::parser::compile;

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

    /// Row limit for default head; default 20. Use 0 for no limit.
    /// (Auto-disabled when -o parquet, so full exports work as expected.)
    #[arg(short = 'n', long, default_value_t = 20)]
    n: usize,

    /// Show the SQL pq would run, but don't execute (for debugging the parser)
    #[arg(long)]
    explain: bool,

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = open_conn()?;

    let fmt = OutputFormat::resolve(&cli.output);

    if let Some(cmd) = cli.command {
        return run_subcommand(&conn, cmd, fmt);
    }

    let file = cli
        .file
        .as_ref()
        .ok_or_else(|| anyhow!("a parquet file is required (try: pq <file>)"))?;
    let query = cli.query.as_deref().unwrap_or("");

    // For parquet export, the user almost never wants the default head LIMIT.
    let effective_n = if fmt == OutputFormat::Parquet {
        0
    } else {
        cli.n
    };
    let sql = compile(file, query, effective_n)?;

    if cli.explain {
        println!("{}", sql);
        return Ok(());
    }

    output::run_and_print(&conn, &sql, fmt)
}

fn open_conn() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("failed to open DuckDB connection")?;
    // Enable cloud httpfs for gs:// / s3:// — duckdb's httpfs is bundled with our build.
    // We swallow errors here because httpfs may already be loaded on some builds.
    let _ = conn.execute_batch(
        r"
        INSTALL httpfs;
        LOAD httpfs;
        ",
    );
    Ok(conn)
}

fn run_subcommand(conn: &Connection, cmd: Cmd, fmt: OutputFormat) -> Result<()> {
    let sql = match cmd {
        Cmd::Schema { file } => format!(
            "SELECT column_name, column_type, \"null\" AS nullable \
             FROM (DESCRIBE SELECT * FROM {src})",
            src = parser::source_clause(&file)
        ),
        Cmd::Stats { file } => format!(
            "SELECT column_name, column_type, min, max, \
                    approx_unique AS approx_distinct, \
                    null_percentage AS null_pct \
             FROM (SUMMARIZE SELECT * FROM {src})",
            src = parser::source_clause(&file)
        ),
        Cmd::Sample { file, n } => format!(
            "SELECT * FROM {src} USING SAMPLE {n} ROWS",
            src = parser::source_clause(&file),
            n = n
        ),
        Cmd::Head { file, n } => format!(
            "SELECT * FROM {src} LIMIT {n}",
            src = parser::source_clause(&file),
            n = n
        ),
        Cmd::Tail { file, n } => format!(
            "WITH t AS (SELECT *, row_number() OVER () AS __rn FROM {src}) \
             SELECT * EXCLUDE (__rn) FROM t \
             ORDER BY __rn DESC LIMIT {n}",
            src = parser::source_clause(&file),
            n = n
        ),
        Cmd::Count { file } => format!(
            "SELECT count(*) AS rows FROM {src}",
            src = parser::source_clause(&file)
        ),
    };

    output::run_and_print(conn, &sql, fmt)
}
