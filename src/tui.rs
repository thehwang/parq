// Interactive TUI mode (`pq tui FILE`).
//
// Design intent (lazygit-flavoured):
//
//   ┌─ Columns ─┐ ┌─ Query (editable, source of truth) ─────────────┐
//   │ ▶ id      │ │ group_by .country | sum .revenue | top 3        │
//   │   email   │ │                                                 │
//   │ ✓ country │ │ ▸ compiled SQL (press : to expand)        12 ms │
//   │ ✓ revenue │ └─────────────────────────────────────────────────┘
//   │   age     │ ┌─ Data (live preview, LIMIT 50) ────────────────┐
//   │           │ │ country │ sum_revenue                          │
//   │ ─ Filters │ │ US      │ 19065.00                             │
//   │ (none)    │ │ FR      │   999.99                             │
//   │           │ │ DE      │   312.00                             │
//   └───────────┘ └────────────────────────────────────────────────┘
//   Tab next pane │ ␣ toggle col │ Y copy CLI │ q quit │ : SQL │ ?
//
// MVP scope (this file):
//   - Editable Query panel as the single source of truth.
//   - Columns panel toggles `.col` projections by rewriting the query string.
//   - Data panel shows whatever the current query produces, capped at 50 rows.
//   - On `q` exit, the equivalent CLI one-liner is written to STDOUT so the
//     user can paste it into a shell history / Makefile / cron.
//   - On `Y`, that one-liner is copied to the clipboard via `arboard`.
//
// Out of scope (v0.6+): semantic cursor sync, explain panel, drill-down,
// query history, multi-file tabs, join builder, schema diff.

use std::env;
use std::fs;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use duckdb::types::Value;
use duckdb::Connection;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};
use ratatui::Terminal;
use tui_textarea::TextArea;

use crate::lineage::{self, Lineage};
use crate::output::value_to_display;
use crate::parser;
use crate::source::InputFormat;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Default cap on the preview pane's row count when the user didn't pass
/// `-n`. Keeping the panel bounded keeps the render path snappy on huge
/// remote files; users wanting more rows pass `pq tui -n 500 file.parquet`.
const PREVIEW_LIMIT_DEFAULT: usize = 50;
const SQL_THROTTLE: Duration = Duration::from_millis(50);
/// Cap on persisted query history. 100 entries fits ~10 KB on disk;
/// browsing 100 is already too many — anyone wanting more should
/// graduate to a fuzzy-search popup (v0.9 idea).
const HISTORY_MAX: usize = 100;

/// Ghost-text shown in the empty Query panel so first-time users see what
/// the DSL accepts. tui-textarea hides it on the first keystroke. Kept
/// short — full grammar lives in `?` help (v0.6) and the README.
const QUERY_PLACEHOLDER: &str =
    "try: '.email, .country where .country == \"US\"' or 'group_by .country | count'";

// ─── Panels ──────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Panel {
    Columns,
    Query,
    Data,
}

impl Panel {
    fn next(self) -> Self {
        match self {
            Panel::Columns => Panel::Query,
            Panel::Query => Panel::Data,
            Panel::Data => Panel::Columns,
        }
    }
    fn prev(self) -> Self {
        match self {
            Panel::Columns => Panel::Data,
            Panel::Query => Panel::Columns,
            Panel::Data => Panel::Query,
        }
    }
}

// ─── Schema (left panel data) ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ColumnInfo {
    name: String,
    ty: String,
    /// Whether the column is part of the current explicit projection.
    /// We re-derive this on every query edit by string-matching `.{name}` —
    /// crude but cheap, and good enough for the MVP. v0.6 will read it from
    /// the parsed QueryPlan directly.
    selected: bool,
}

fn fetch_schema(conn: &Connection, file: &str, fmt: InputFormat) -> Result<Vec<ColumnInfo>> {
    // v0.11: route through the format-aware reader so `pq tui -i ndjson f.ndjson`
    // and `pq tui -i csv f.csv` get the right `read_json` / `read_csv_auto`.
    // Before this the schema panel was hard-wired to read_parquet and would
    // silently throw a binder error on non-parquet sources.
    let src = parser::source_clause_fmt(file, fmt);
    let sql = format!("SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM {src})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let ty: String = row.get(1)?;
        out.push(ColumnInfo {
            name,
            ty,
            selected: false,
        });
    }
    Ok(out)
}

// ─── Preview (right-bottom panel data) ───────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct Preview {
    /// Column headers from the most recent successful query.
    headers: Vec<String>,
    /// Up to `App::preview_limit` rows; each cell is preformatted text.
    rows: Vec<Vec<String>>,
    /// Compiled SQL (for the `:` SQL viewer).
    sql: String,
    /// Wall-clock spent in execute() — shown in the Query panel header.
    last_ms: u128,
    /// Set when the most recent compile/execute failed; cleared on next success.
    error: Option<String>,
    /// Parsed snapshot of `EXPLAIN <sql>` for the Explain panel and
    /// suggestion engine. None when the explain itself errored (which
    /// is the same code path that already populated `error`).
    explain: Option<ExplainSummary>,
}

/// Distilled view of DuckDB's EXPLAIN output — what we need to drive the
/// Explain panel and the heuristic-suggestion engine, and nothing more.
/// Filled by `run_explain` (cheap, just-the-plan) or `run_analyze`
/// (executes the full query for actuals + per-op timing) from a forgiving
/// text-grep over the plan tree; missing fields just mean the heuristic
/// didn't fire / wasn't observable in this particular plan.
#[derive(Debug, Clone, Default)]
struct ExplainSummary {
    /// One entry per data-source scan node (PARQUET_SCAN / READ_PARQUET /
    /// TABLE_SCAN). Joins produce two scans, simple queries one.
    scans: Vec<ScanInfo>,
    /// Free-form actionable hints: "💡 add `where .dt = …` to prune row
    /// groups", "💡 select fewer columns", etc. Generated by
    /// `gen_suggestions` from `scans` + the source path + the user's DSL.
    suggestions: Vec<String>,
    /// Raw EXPLAIN text. We render this as-is when the panel has no
    /// structured facts to surface, so the user still sees *something*
    /// rather than an empty box.
    raw: String,
    /// True when this summary came from `EXPLAIN ANALYZE` (full execute)
    /// rather than plain `EXPLAIN` (estimates only). Drives the panel
    /// header badge and the actual-vs-estimate suggestion logic.
    analyzed: bool,
    /// Wall-clock from `Total Time: X.Ys` at the top of the ANALYZE tree.
    /// None for plain EXPLAIN. Seconds, not ms — DuckDB's native unit.
    total_seconds: Option<f64>,
}

/// Handle to an in-flight EXPLAIN ANALYZE worker. Owns the receiver
/// end of an mpsc channel; the worker thread holds the matching
/// sender. We deliberately don't carry the worker's JoinHandle —
/// when the user cancels (drops `analyze_job`), we just orphan the
/// thread. It'll send into a disconnected tx, ignore the error, and
/// exit. Cheap, no unsafe, no extra DuckDB plumbing.
// AnalyzeJob can't `derive(Debug)` because duckdb's InterruptHandle
// doesn't implement Debug; carry the field as-is and skip the derive.
struct AnalyzeJob {
    /// Result channel — `try_recv` non-blockingly each tick.
    rx: mpsc::Receiver<ExplainSummary>,
    /// Stamp used by the panel header to render
    /// "ANALYZE running… 1.2 s" so the user can tell whether it's
    /// hung vs just slow.
    started_at: Instant,
    /// v0.12: handle to the worker's DuckDB connection. Lets the main
    /// thread call `interrupt()` when the user hits Ctrl-C — DuckDB
    /// then returns an INTERRUPT error from its current parquet scan
    /// and the worker exits within milliseconds. Without this, the
    /// worker would keep grinding through a multi-GB file even after
    /// the user moved on.
    interrupt: std::sync::Arc<duckdb::InterruptHandle>,
}

/// v0.13: in-flight preview worker. Mirrors AnalyzeJob — same
/// pattern, separate connection, separate interrupt handle. Pre-v0.13
/// the preview ran synchronously inside `maybe_run_compile`, which
/// meant that typing a query against a 12 GB file would freeze the
/// entire TUI for the duration of the scan; not even Ctrl-C got
/// processed because the event loop wasn't running.
struct PreviewJob {
    rx: mpsc::Receiver<Preview>,
    started_at: Instant,
    interrupt: std::sync::Arc<duckdb::InterruptHandle>,
    /// Snapshot of the query text the worker is running. We use this
    /// to decide whether the result is still relevant when it lands
    /// (the user may have typed more by then) — primarily useful for
    /// the history-recording side-effect, which should only fire when
    /// the result we got back actually matches the current query.
    query_when_started: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ScanInfo {
    /// Filters that DuckDB pushed down into the scan — visible in EXPLAIN
    /// as `Filters: a > 10`. Empty means no predicate pushdown happened.
    filters: Vec<String>,
    /// Columns the scan reads (projection pushdown). Empty means the
    /// optimizer didn't surface a Projections line — usually equivalent
    /// to "scan returns all columns".
    projections: Vec<String>,
    /// Optimizer's row-count estimate (from plain `EXPLAIN`). Visible in
    /// the tree as `~N rows` or the older `Estimated Cardinality: N`.
    estimated_rows: Option<u64>,
    /// Actual rows the scan emitted at runtime. Only populated by
    /// `EXPLAIN ANALYZE`; visible in the tree as a bare `N rows` line
    /// (without the `~` prefix that marks an estimate).
    actual_rows: Option<u64>,
    /// `Total Files Read: N` line — useful when querying a glob that
    /// matched many parquet files; lets us call out "scanning every
    /// file" suggestions later.
    files_read: Option<u64>,
    /// v0.14 — physical rows the scan actually read after row-group
    /// pruning. Pulled from DuckDB's JSON profile (`operator_rows_scanned`
    /// on the READ_PARQUET node). Only populated by `EXPLAIN ANALYZE`,
    /// and only when DuckDB version supports the JSON profile shape.
    rows_scanned: Option<u64>,
    /// v0.14 — total rows in the parquet file(s) backing this scan, from
    /// `parquet_file_metadata`. Glob paths sum across matched files.
    /// Combined with `rows_scanned` to compute `pruning_ratio`.
    file_total_rows: Option<u64>,
    /// v0.14 — fraction of rows that DuckDB's row-group pruner skipped:
    /// `1.0 - rows_scanned / file_total_rows`, clamped to 0.0..=1.0.
    /// 0.0 means the pruner couldn't skip anything (e.g. predicate isn't
    /// on a column with min/max stats); 0.8 means 80% of rows never
    /// touched decompression. None when the JSON profile path didn't
    /// produce data for this scan.
    pruning_ratio: Option<f64>,
}

fn run_preview(
    conn: &Connection,
    file: &str,
    query: &str,
    fmt: InputFormat,
    preview_limit: usize,
) -> Preview {
    let started = Instant::now();
    // v0.11: format-aware compile so non-parquet sources work. Without
    // this the TUI built `read_parquet('foo.ndjson')` and the user got
    // a binder error in the Query panel header — the same query worked
    // fine from the CLI because main.rs already routes through
    // compile_plan_fmt.
    let plan = match parser::compile_plan_fmt(file, query, preview_limit, fmt) {
        Ok(p) => p,
        Err(e) => {
            return Preview {
                error: Some(format!("parse: {e:#}")),
                ..Default::default()
            }
        }
    };
    // Inject a hard outer LIMIT in case the user wrote a query with no limit.
    // Cheap to wrap; DuckDB's optimizer collapses double LIMITs.
    let sql_with_cap = format!(
        "SELECT * FROM ({}) AS __pq_tui_preview LIMIT {}",
        plan.sql, preview_limit
    );

    let mut stmt = match conn.prepare(&sql_with_cap) {
        Ok(s) => s,
        Err(e) => {
            return Preview {
                sql: plan.sql,
                error: Some(format!("compile: {e}")),
                ..Default::default()
            }
        }
    };
    let mut rows_iter = match stmt.query([]) {
        Ok(r) => r,
        Err(e) => {
            return Preview {
                sql: plan.sql,
                error: Some(format!("execute: {e}")),
                ..Default::default()
            }
        }
    };
    let headers: Vec<String> = rows_iter
        .as_ref()
        .map(|s| s.column_names())
        .unwrap_or_default();
    let ncols = headers.len();

    let mut rows = Vec::with_capacity(preview_limit);
    while let Ok(Some(row)) = rows_iter.next() {
        let mut cells = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: Value = row.get(i).unwrap_or(Value::Null);
            // v0.11: use the shared output::value_to_display so TIMESTAMP /
            // STRUCT / LIST / MAP cells render as ISO strings / compact JSON
            // instead of Rust Debug output (`Timestamp(Microsecond, …)` etc.).
            cells.push(value_to_display(&v));
        }
        rows.push(cells);
    }
    // EXPLAIN runs on the inner `plan.sql` (not the LIMIT-50-wrapped variant)
    // so the cardinality estimates and pushdown decisions reflect the
    // user's actual query, not the TUI's preview cap. EXPLAIN never
    // executes, so it's cheap (~sub-ms even on remote parquet) — fine to
    // run on every preview tick.
    let explain = run_explain(conn, &plan.sql, file, query);
    Preview {
        headers,
        rows,
        sql: plan.sql,
        last_ms: started.elapsed().as_millis(),
        error: None,
        explain,
    }
}

/// Run `EXPLAIN <sql>` against the connection and distil it into an
/// `ExplainSummary`. Returns None when the explain itself errors (same
/// code path that already populated Preview.error) — the panel falls
/// back to "(query failed — fix it to see plan)" in that case.
fn run_explain(conn: &Connection, sql: &str, file: &str, query: &str) -> Option<ExplainSummary> {
    let mut text = String::new();
    let mut stmt = match conn.prepare(&format!("EXPLAIN {sql}")) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return None,
    };
    while let Ok(Some(r)) = rows.next() {
        // Each EXPLAIN row is (explain_key, explain_value). We concatenate
        // every value (logical_plan, physical_plan, etc.) so our grep can
        // find scan info regardless of which plan layer DuckDB returns.
        if let Ok(v) = r.get::<_, String>(1) {
            text.push_str(&v);
            text.push('\n');
        }
    }
    let (scans, total_seconds) = parse_explain(&text);
    let analyzed = false;
    let suggestions = gen_suggestions(&scans, file, query, analyzed);
    Some(ExplainSummary {
        scans,
        suggestions,
        raw: text,
        analyzed,
        total_seconds,
    })
}

/// v0.14 — walk a DuckDB JSON profile (`enable_profiling='json'` shape)
/// and collect (rows_scanned, filename) for every READ_PARQUET node.
/// Returns nodes in plan order (depth-first), matching what
/// `parse_explain` emits from the box-drawing text plan; this lets the
/// caller merge by index without filename matching.
fn collect_json_parquet_scans(node: &serde_json::Value) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    walk_json_for_parquet(node, &mut out);
    out
}

fn walk_json_for_parquet(node: &serde_json::Value, out: &mut Vec<(u64, String)>) {
    let is_parquet = node
        .get("operator_name")
        .and_then(|v| v.as_str())
        .map(|s| s == "READ_PARQUET")
        .unwrap_or(false);
    if is_parquet {
        // v0.14.1 (#12 follow-up): use `operator_cardinality`, NOT
        // `operator_rows_scanned`. Empirically against DuckDB 1.10.501,
        // `operator_rows_scanned` for READ_PARQUET is ~10x the file's
        // physical row count (likely an internal multi-pass / per-thread
        // accumulator), which produced the comically wrong "pruned: 0%
        // (25.0M/5.0M rows)" line on the v0.14 cover image.
        // `operator_cardinality` is the rows-out-of-scan count after
        // predicate pushdown — exactly the numerator we want for the
        // pruning gauge: (1 - cardinality / file_total_rows).
        let rows = node.get("operator_cardinality").and_then(|v| v.as_u64());
        let filename = node
            .get("extra_info")
            .and_then(|e| e.get("Filename(s)"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let (Some(r), Some(f)) = (rows, filename) {
            out.push((r, f));
        }
    }
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for c in children {
            walk_json_for_parquet(c, out);
        }
    }
}

/// v0.14 — best-effort JSON profile of `EXPLAIN ANALYZE <sql>`.
///
/// DuckDB's `enable_profiling='json'` PRAGMA changes the format of
/// EXPLAIN ANALYZE output: column 1 of the result row becomes a JSON
/// document instead of the box-drawing text plan. We toggle the PRAGMA,
/// run analyze, parse the JSON, then ALWAYS reset the PRAGMA so
/// subsequent preview ticks on this connection get the normal text plan.
///
/// Returns None on any failure (older DuckDB without this profile shape,
/// malformed JSON, etc.) — callers fall back to the text-only path.
fn try_collect_json_scans(conn: &Connection, sql: &str) -> Option<Vec<(u64, String)>> {
    let _ = conn.execute("PRAGMA enable_profiling='json'", []);
    let _ = conn.execute("PRAGMA profiling_mode='detailed'", []);
    let result: Option<Vec<(u64, String)>> = (|| {
        let mut stmt = conn.prepare(&format!("EXPLAIN ANALYZE {sql}")).ok()?;
        let mut rows = stmt.query([]).ok()?;
        let row = rows.next().ok()??;
        let json_str: String = row.get(1).ok()?;
        let value: serde_json::Value = serde_json::from_str(&json_str).ok()?;
        Some(collect_json_parquet_scans(&value))
    })();
    // Always restore default profiling state so subsequent queries
    // (preview ticks, suggestions sql, etc.) don't get JSON output
    // shoved in their first column.
    //
    // v0.14.1 (#12): `PRAGMA disable_profiling` looks like the obvious
    // reset but it's a silent no-op against DuckDB 1.10.501 — the
    // pragma accepts the call without erroring but EXPLAIN ANALYZE
    // keeps returning JSON in column 1. The documented inverse is
    // `enable_profiling='no_output'`, which actually flips the bit.
    // Verified empirically; the regression test below pins this.
    let _ = conn.execute("PRAGMA enable_profiling='no_output'", []);
    let _ = conn.execute("PRAGMA profiling_mode='standard'", []);
    result
}

/// v0.14 — merge JSON-derived pruning data into text-parsed scans.
/// Matches by **index**: nth READ_PARQUET in the JSON tree maps to the
/// nth ScanInfo from parse_explain. parse_explain walks the box-drawing
/// text top-to-bottom and the JSON tree is depth-first child-order, so
/// for the queries pq supports (single source, simple joins) the orders
/// align. If the lengths disagree we conservatively skip the merge —
/// better to show no pruning info than mis-attribute it to the wrong scan.
fn merge_pruning_metrics(scans: &mut [ScanInfo], json_scans: &[(u64, String)], conn: &Connection) {
    if scans.len() != json_scans.len() {
        return;
    }
    for (scan, (rows_scanned, filename)) in scans.iter_mut().zip(json_scans.iter()) {
        // parquet_file_metadata accepts both single paths and globs;
        // we pass the Filename(s) value through unchanged. On any
        // failure (cloud paths, missing files, non-parquet) we leave
        // pruning_ratio as None.
        let total = file_total_rows(conn, filename);
        scan.rows_scanned = Some(*rows_scanned);
        if let Some(t) = total {
            scan.file_total_rows = Some(t);
            scan.pruning_ratio = Some(pruning_ratio(*rows_scanned, t));
        }
    }
}

/// Sum row counts across every row group of every parquet file matched
/// by `path` (a single path or a glob both work). None on error.
fn file_total_rows(conn: &Connection, path: &str) -> Option<u64> {
    if path.is_empty() {
        return None;
    }
    // We don't support cloud paths in lite mode for the same reason
    // count_sql doesn't: parquet_file_metadata over httpfs is unreliable.
    if path.contains("://") {
        return None;
    }
    let mut stmt = conn
        .prepare("SELECT COALESCE(sum(num_rows), 0) FROM parquet_file_metadata(?)")
        .ok()?;
    let mut rows = stmt.query(duckdb::params![path]).ok()?;
    let row = rows.next().ok()??;
    let total: u64 = row.get(0).ok()?;
    if total == 0 {
        None
    } else {
        Some(total)
    }
}

/// Clamp pruning ratio to a sane 0.0..=1.0 range. operator_rows_scanned
/// occasionally exceeds total_rows by a tiny margin (parallelism /
/// double-counting in earlier DuckDB versions); without clamping that
/// would give a negative ratio that renders nonsensically.
fn pruning_ratio(rows_scanned: u64, file_total: u64) -> f64 {
    if file_total == 0 {
        return 0.0;
    }
    let raw = 1.0 - (rows_scanned as f64) / (file_total as f64);
    raw.clamp(0.0, 1.0)
}

/// Run `EXPLAIN ANALYZE <sql>` on the *full* query (no LIMIT 50 wrapper)
/// and return a summary populated with actual-runtime numbers.
///
/// Caller must accept that this **executes** the query — fast on small
/// local files (sub-ms), slow on remote/big ones. Bound to a power-user
/// keypress (capital `E`) so it never fires by accident.
///
/// v0.14: after collecting the text plan we make a *second* best-effort
/// EXPLAIN ANALYZE pass with `enable_profiling='json'` to pull row-group
/// pruning metrics. This doubles ANALYZE time on the chosen query but
/// the user opted in via capital-E so the cost is acceptable.
fn run_analyze(conn: &Connection, sql: &str, file: &str, query: &str) -> Option<ExplainSummary> {
    let mut text = String::new();
    let mut stmt = match conn.prepare(&format!("EXPLAIN ANALYZE {sql}")) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return None,
    };
    while let Ok(Some(r)) = rows.next() {
        if let Ok(v) = r.get::<_, String>(1) {
            text.push_str(&v);
            text.push('\n');
        }
    }
    let (mut scans, total_seconds) = parse_explain(&text);
    // Best-effort JSON profile path. Failures are silently swallowed —
    // the panel just shows the same content it always did, minus the
    // pruning ratio rows.
    if let Some(json_scans) = try_collect_json_scans(conn, sql) {
        merge_pruning_metrics(&mut scans, &json_scans, conn);
    }
    let analyzed = true;
    let suggestions = gen_suggestions(&scans, file, query, analyzed);
    Some(ExplainSummary {
        scans,
        suggestions,
        raw: text,
        analyzed,
        total_seconds,
    })
}

/// Tolerantly parse DuckDB's ASCII-art EXPLAIN (or EXPLAIN ANALYZE) tree.
/// Returns `(scans, total_seconds)` — the latter set only by ANALYZE
/// runs that emit a `Total Time: X.Ys` line at the top.
///
/// We strip box-drawing chars first, then walk lines as a tiny state
/// machine: operator labels (UPPER_SNAKE_CASE) bracket sections; inside
/// a scan section we collect Filters/Projections/cardinality lines.
///
/// **Quirks the design has to survive:**
/// - Plain EXPLAIN labels parquet scans `READ_PARQUET`; ANALYZE labels
///   the same thing `TABLE_SCAN` and parks the function name under a
///   `Function: READ_PARQUET` field. We treat all three label variants
///   as scan boundaries.
/// - Each scan node embeds its function name as a **value** under a
///   `Function:` label — meaning `READ_PARQUET` shows up twice per scan,
///   once as the operator title and once as the function-name field.
///   We disambiguate by tracking the previous non-blank line: when that
///   line ends with `:`, the next operator-shaped line is a value, not
///   a new operator boundary.
/// - Cardinality has appeared in three shapes across DuckDB versions:
///   `Estimated Cardinality: 248` (oldest), `~248 rows` (newer
///   estimates), and bare `248 rows` (ANALYZE actuals — note the
///   missing `~`). We route each to the right ScanInfo field.
fn parse_explain(plan: &str) -> (Vec<ScanInfo>, Option<f64>) {
    let mut out: Vec<ScanInfo> = Vec::new();
    let mut current: Option<ScanInfo> = None;
    let mut prev_nonblank = String::new();
    let mut total_seconds: Option<f64> = None;

    for raw in plan.lines() {
        // Strip every box-drawing char so we can do simple `starts_with`
        // checks. The trailing `.trim()` also drops leading whitespace
        // that DuckDB uses for indentation.
        let line: String = raw
            .chars()
            .filter(|c| !"│─┌┐└┘├┤┬┴┼".contains(*c))
            .collect();
        let line = line.trim();
        if line.is_empty() {
            // Don't reset prev_nonblank — blank lines inside a node
            // shouldn't break the Function:→VALUE disambiguation below.
            continue;
        }

        // Top-level "Total Time: 0.0052s" appears in ANALYZE output even
        // before any operator boxes — pick it up wherever we see it
        // (always, not gated on being inside/outside a scan).
        if let Some(rest) = line.strip_prefix("Total Time:") {
            let trimmed = rest.trim().trim_end_matches('s').trim();
            if let Ok(s) = trimmed.parse::<f64>() {
                total_seconds = Some(s);
            }
            // Don't `continue` — fall through; "Total Time: …" is also
            // a key-ending-in-colon, which informs the next line's
            // is_field_value disambiguation correctly.
        }

        // Heuristic operator-label test: all uppercase / underscore /
        // digit, with at least one uppercase letter (so a bare number
        // like `42` doesn't masquerade as an operator).
        let is_op_label = line
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
            && line.chars().any(|c| c.is_ascii_uppercase());
        // Skip the inner `Function: READ_PARQUET` shape — that second
        // `READ_PARQUET` is a field value, not an operator title.
        let is_field_value = prev_nonblank.ends_with(':');
        // Recognise every name DuckDB has used for the parquet read
        // operator across versions. TABLE_SCAN appears in ANALYZE
        // output, READ_PARQUET in plain EXPLAIN, PARQUET_SCAN in older
        // builds.
        let is_scan_label =
            line.contains("PARQUET_SCAN") || line.contains("READ_PARQUET") || line == "TABLE_SCAN";

        if is_op_label && !is_field_value {
            // Close any open scan; open a fresh one if this label is
            // itself a parquet scan (otherwise we just drop through).
            if let Some(s) = current.take() {
                out.push(s);
            }
            if is_scan_label {
                current = Some(ScanInfo::default());
            }
            prev_nonblank = line.to_string();
            continue;
        }

        // Inside a scan: collect known fields.
        if let Some(s) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("Filters:") {
                let body = rest.trim();
                if !body.is_empty() {
                    s.filters.push(body.to_string());
                }
            } else if let Some(rest) = line.strip_prefix("Projections:") {
                s.projections.extend(
                    rest.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty()),
                );
            } else if let Some(rest) = line.strip_prefix("Estimated Cardinality:") {
                if let Ok(n) = rest.trim().replace(',', "").parse::<u64>() {
                    s.estimated_rows = Some(n);
                }
            } else if let Some(rest) = line.strip_prefix("Total Files Read:") {
                if let Ok(n) = rest.trim().replace(',', "").parse::<u64>() {
                    s.files_read = Some(n);
                }
            } else if let Some(rest) = line.strip_prefix('~') {
                // Newer DuckDB estimate: "~248 rows" / "~1 row" (commas tolerated).
                let trimmed = rest
                    .trim()
                    .trim_end_matches("rows")
                    .trim_end_matches("row")
                    .trim();
                if let Ok(n) = trimmed.replace(',', "").parse::<u64>() {
                    s.estimated_rows = Some(n);
                }
            } else if line.ends_with(" rows") || line.ends_with(" row") {
                // ANALYZE actuals: bare "248 rows" / "1 row" — no `~` prefix.
                // We're only interested in lines that are pure number+unit;
                // skip anything that's also got Filters/Projections content
                // (those branches above already returned above).
                let trimmed = line.trim_end_matches("rows").trim_end_matches("row").trim();
                if let Ok(n) = trimmed.replace(',', "").parse::<u64>() {
                    s.actual_rows = Some(n);
                }
            }
        }

        prev_nonblank = line.to_string();
    }
    if let Some(s) = current {
        out.push(s);
    }
    (out, total_seconds)
}

/// Generate actionable performance hints from the scan facts + source
/// path + user's DSL. Each hint is a single line, prefixed with 💡 so
/// the panel renderer can style them as a group. Order: cheapest wins
/// first (filtering > projection > limit).
///
/// `analyzed` is true when the scan facts came from `EXPLAIN ANALYZE`
/// (so `actual_rows` and `total_seconds` are present); we only emit the
/// stats-divergence hint in that case.
fn gen_suggestions(scans: &[ScanInfo], file: &str, query: &str, analyzed: bool) -> Vec<String> {
    let mut out = Vec::new();
    let q_lower = query.to_ascii_lowercase();

    // Hive partitions present in the path but not referenced by the query
    // → row groups can't be pruned. This is the highest-impact win on
    // most warehouses (pruning a partition often skips 99% of bytes).
    for k in extract_hive_keys(file) {
        let needle = format!(".{}", k.to_ascii_lowercase());
        if !q_lower.contains(&needle) {
            out.push(format!(
                "💡 add `where {needle} = …` to prune partitions ({k}=… in path)"
            ));
        }
    }

    // No predicate pushdown on any scan — query has no WHERE that the
    // optimizer could push, OR the predicate isn't on a scan column.
    // Skip this hint for trivial empty queries (default LIMIT 20 preview
    // doesn't need a filter).
    let any_filter_pushed = scans.iter().any(|s| !s.filters.is_empty());
    if !query.trim().is_empty()
        && !any_filter_pushed
        && (q_lower.contains(" where ") || q_lower.starts_with("where "))
    {
        out.push(
            "💡 your `where` clause didn't push to the parquet scan — \
             check column types match"
                .into(),
        );
    }

    // No projection pushdown + bare query → reading all columns wastes
    // I/O on wide tables. We don't know the source's total column count
    // from EXPLAIN alone; the App passes the schema separately, but we
    // keep this function pure by checking the simpler "user wrote no
    // explicit projection" condition.
    let any_proj_pushed = scans.iter().any(|s| !s.projections.is_empty());
    if !any_proj_pushed && (query.trim().is_empty() || !q_lower.contains('.')) {
        out.push(
            "💡 select specific columns (`.col1, .col2`) for projection pushdown — \
             scans currently read every field"
                .into(),
        );
    }

    // ANALYZE-only hints. These need actual row counts, so they only
    // fire when the user ran EXPLAIN ANALYZE (capital `E`).
    if analyzed {
        for s in scans {
            // Optimizer estimate diverged from reality by >10x in either
            // direction. This is usually stale parquet stats — newer
            // writes after the metadata footer was generated. Common on
            // append-only datalakes.
            if let (Some(est), Some(act)) = (s.estimated_rows, s.actual_rows) {
                let est_f = est.max(1) as f64;
                let act_f = act.max(1) as f64;
                let ratio = (act_f / est_f).max(est_f / act_f);
                if ratio >= 10.0 {
                    out.push(format!(
                        "💡 estimate skewed {ratio:.0}× (est {est}, actual {act}) — \
                         parquet stats may be stale; rewriting the file refreshes them"
                    ));
                }
            }
            // Many files scanned with no `where` predicate that pushed →
            // probably a glob over a partitioned dataset that's reading
            // every file because nothing prunes.
            if let Some(n) = s.files_read {
                if n >= 20 && s.filters.is_empty() {
                    out.push(format!(
                        "💡 scanned {n} files with no pushed predicate — \
                         add a `where` on a partition column to prune"
                    ));
                }
            }
            // v0.14: pruning ratio is exactly 0 despite the user pushing
            // a predicate. Almost always means the predicate column has
            // no min/max stats in the parquet footer (very common for
            // STRING columns from older Spark writers that don't write
            // dictionary-page stats). Only fire on files big enough to
            // matter — small parquets have one row group anyway, no
            // pruning is possible.
            const PRUNE_HINT_MIN_ROWS: u64 = 1_000_000;
            if let (Some(0.0..=0.0), false, Some(total)) =
                (s.pruning_ratio, s.filters.is_empty(), s.file_total_rows)
            {
                if total >= PRUNE_HINT_MIN_ROWS {
                    let filt = s.filters.join(" AND ");
                    out.push(format!(
                        "💡 filter `{filt}` didn't prune any row groups — \
                         column may lack min/max stats (common for STRING \
                         from older Spark writers)"
                    ));
                }
            }
        }
    }

    out
}

/// Extract hive partition KEYS from a path. Mirrors the heuristic in
/// `parser::looks_like_hive_partition` but returns the keys themselves
/// so we can name them in suggestions ("add `where .dt = …`"). Same
/// segment grammar: `key=value` where key is alphanumeric_/-.
fn extract_hive_keys(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in path.split('/') {
        if let Some(eq) = seg.find('=') {
            let k = &seg[..eq];
            let v = &seg[eq + 1..];
            let key_ok = !k.is_empty()
                && !v.is_empty()
                && k.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if key_ok && !out.contains(&k.to_string()) {
                out.push(k.to_string());
            }
        }
    }
    out
}

// (TUI used to ship its own `value_to_string` here — a stripped-down copy of
// output::value_to_display. v0.11 unified them: TIMESTAMP / TIME / nested
// types render the same in `pq` and `pq tui`. The shared implementation
// lives in `output.rs` so adding a new type rendering only touches one place.)

// ─── App state ───────────────────────────────────────────────────────────────

struct App<'ta> {
    file: String,
    /// Input format passed in from the CLI (`-i parquet|ndjson|csv`, sniffed
    /// from extension if `auto`). Threaded into `compile_plan_fmt` /
    /// `source_clause_fmt` so non-parquet inputs DTRT in the schema panel,
    /// preview, and EXPLAIN/ANALYZE worker.
    input_fmt: InputFormat,
    /// Cap on rows shown in the Data panel; from `-n` (default
    /// PREVIEW_LIMIT_DEFAULT). Also doubles as the LIMIT injected when the
    /// user's query has no explicit limit.
    preview_limit: usize,
    /// `--udf` macro definitions captured from the CLI. Re-registered on
    /// every fresh DuckDB connection (the main one + every ANALYZE worker)
    /// so user-defined functions referenced in queries always resolve.
    udfs: Vec<String>,
    columns: Vec<ColumnInfo>,
    column_state: ListState,
    query: TextArea<'ta>,
    preview: Preview,
    focus: Panel,
    /// True when `:` is pressed → expands the compiled-SQL panel.
    show_sql: bool,
    /// True when `e` is pressed (in Columns/Data — `e` is a typed char in
    /// Query). Surfaces the Explain panel below Data: scan summary,
    /// pushdown facts, and 💡 heuristic suggestions.
    show_explain: bool,
    /// In-flight EXPLAIN ANALYZE worker. Set when capital `E` is
    /// pressed; cleared when the worker delivers a result OR the user
    /// hits `Esc` to abandon it. Runs on a dedicated thread so the TUI
    /// stays responsive — even on remote/large parquet files where
    /// ANALYZE can take seconds.
    analyze_job: Option<AnalyzeJob>,
    /// v0.13: in-flight preview worker. Set whenever the throttle
    /// fires and we kick off a fresh `run_preview` on a worker
    /// thread; cleared when the result lands (or the user types
    /// more, which interrupts and overwrites). Decouples the event
    /// loop from query latency so the TUI never freezes on a slow
    /// scan.
    preview_job: Option<PreviewJob>,
    /// True when `?` is pressed → renders a centred help overlay over the
    /// 4-panel layout. Any subsequent key dismisses it (so `?` then start
    /// typing in Query feels natural).
    show_help: bool,
    /// Last edited query text — cached so we don't re-run on focus changes.
    last_compiled: String,
    /// Set when query was edited but we're throttling SQL execution.
    pending_compile_at: Option<Instant>,
    /// Tiny status message in the bottom bar (e.g. "✓ copied to clipboard").
    flash: Option<(String, Instant)>,
    /// Set to true when the user presses `q` / Ctrl-C.
    should_quit: bool,
    /// Set on quit when we want to print the equivalent CLI to stdout.
    print_cli_on_exit: bool,

    // ── v0.8: query history navigation (Ctrl-↑ / Ctrl-↓) ────────────────────
    /// Persisted DSL queries from prior sessions, newest first. Loaded
    /// once at startup from ~/.pq/history; appended to whenever a new
    /// distinct compile lands. Capped at HISTORY_MAX so the file doesn't
    /// grow without bound.
    history: Vec<String>,
    /// Active history-cursor position when the user is paging via
    /// Ctrl-↑/Ctrl-↓. None means "I'm typing fresh, history is dormant"
    /// — the next Ctrl-↑ saves the textarea contents into `history_draft`
    /// and starts paging from index 0.
    history_idx: Option<usize>,
    /// Snapshot of whatever the user had typed when they kicked off
    /// history paging. Restored when they Ctrl-↓ all the way back to
    /// the present, so navigating history is non-destructive.
    history_draft: Option<String>,

    // ── v0.6: semantic sync + completion ────────────────────────────────────
    //
    /// Token-level lineage of the current query. Recomputed on every
    /// keystroke (cheap: pure scan of the query string).
    lineage: Lineage,
    /// Column-cursor inside the Data panel. None until the user moves with
    /// ←/→ or h/l; restored from `Some(0)` on first nav so the highlight
    /// has a deterministic starting place. Used to derive `active_source`
    /// when the Data panel is focused (column header → source field).
    data_col_idx: Option<usize>,
    /// Row-cursor inside the Data panel — paired with data_col_idx but
    /// orthogonal: the row picks which sample to use for drill-down (Enter
    /// in Data panel), the column drives lineage highlighting. Initialized
    /// to None and seeded to Some(0) on the first ↑/↓ press.
    data_row_idx: Option<usize>,
    /// Pre-drill-down snapshot of the query buffer. Drill-down replaces the
    /// query with `where .col == val [AND ...]`, which is irreversible
    /// from inside tui-textarea (we rebuild the textarea, losing its
    /// internal undo stack). Stashing the prior query here lets Backspace
    /// in the Data panel pop the user back out — single-level undo, but
    /// that's enough for the "drill-in / drill-out" loop analysts use.
    drill_undo: Option<String>,
    /// In-flight schema completion. Some(_) iff the cursor is currently
    /// just after a `.prefix` pattern *and* one or more schema columns
    /// match. Rendering draws a popup; key dispatch routes Up/Down/Enter
    /// here instead of forwarding to the textarea.
    completion: Option<Completion>,
}

/// Schema-completion popup state. Lives only as long as the user is mid-typing
/// a column reference — created on every keystroke that lands in `.prefix`
/// position and torn down as soon as the prefix vanishes (cursor moves out
/// of the dot-token, prefix matches no schema columns, or popup is dismissed
/// with Esc).
#[derive(Debug, Clone)]
struct Completion {
    /// What the user has typed after the `.` so far, in original case.
    /// Used to highlight the matched prefix inside each candidate row.
    prefix: String,
    /// Byte offset of the leading `.` in the query buffer — needed when
    /// inserting a candidate (we replace `.<prefix>` with `.<full>`).
    dot_byte: usize,
    /// Schema column names matching the prefix, ordered by relevance:
    /// exact-prefix matches first (case-insensitive), then substring matches.
    candidates: Vec<String>,
    /// Index into `candidates` for the currently highlighted row.
    selected: usize,
}

impl<'ta> App<'ta> {
    fn new(
        file: String,
        conn: &Connection,
        input_fmt: InputFormat,
        preview_limit: usize,
        udfs: Vec<String>,
    ) -> Result<Self> {
        let columns = fetch_schema(conn, &file, input_fmt)?;
        let mut column_state = ListState::default();
        if !columns.is_empty() {
            column_state.select(Some(0));
        }

        let mut query = TextArea::default();
        query.set_block(Block::default().borders(Borders::ALL).title(" Query "));
        // Show a dim ghost-line as placeholder. tui-textarea draws it only
        // while the buffer is empty; first keystroke removes it. The hint
        // surfaces the most common DSL shape so first-time users have
        // something to delete-and-replace instead of staring at a blank.
        query.set_placeholder_text(QUERY_PLACEHOLDER);
        query.set_placeholder_style(Style::default().fg(Color::DarkGray));
        // Default: show first preview_limit rows. We leave the textarea
        // empty; compile_plan_fmt expands an empty query into
        // `SELECT * FROM ... LIMIT preview_limit`.

        let preview = run_preview(conn, &file, "", input_fmt, preview_limit);
        Ok(Self {
            file,
            input_fmt,
            preview_limit,
            udfs,
            columns,
            column_state,
            query,
            preview,
            focus: Panel::Columns,
            show_sql: false,
            show_explain: false,
            analyze_job: None,
            preview_job: None,
            show_help: false,
            last_compiled: String::new(),
            pending_compile_at: None,
            flash: None,
            should_quit: false,
            print_cli_on_exit: false,
            history: load_history(),
            history_idx: None,
            history_draft: None,
            lineage: Lineage::default(),
            data_col_idx: None,
            data_row_idx: None,
            drill_undo: None,
            completion: None,
        })
    }

    /// Test-only constructor: build an App with no DuckDB connection
    /// and a hardcoded schema/preview. Used by the v0.8 snapshot tests
    /// to feed `render` deterministic state without spinning up DuckDB
    /// or hitting the filesystem.
    #[cfg(test)]
    fn for_test(file: impl Into<String>, columns: Vec<ColumnInfo>, preview: Preview) -> Self {
        let mut column_state = ListState::default();
        if !columns.is_empty() {
            column_state.select(Some(0));
        }
        let mut query = TextArea::default();
        query.set_block(Block::default().borders(Borders::ALL).title(" Query "));
        query.set_placeholder_text(QUERY_PLACEHOLDER);
        query.set_placeholder_style(Style::default().fg(Color::DarkGray));
        Self {
            file: file.into(),
            input_fmt: InputFormat::Parquet,
            preview_limit: PREVIEW_LIMIT_DEFAULT,
            udfs: Vec::new(),
            columns,
            column_state,
            query,
            preview,
            focus: Panel::Columns,
            show_sql: false,
            show_explain: false,
            analyze_job: None,
            preview_job: None,
            show_help: false,
            last_compiled: String::new(),
            pending_compile_at: None,
            flash: None,
            should_quit: false,
            print_cli_on_exit: false,
            // Snapshot tests must NOT pull in the user's real history
            // file — keep the buffer empty so we get reproducible output.
            history: Vec::new(),
            history_idx: None,
            history_draft: None,
            lineage: Lineage::default(),
            data_col_idx: None,
            data_row_idx: None,
            drill_undo: None,
            completion: None,
        }
    }

    fn current_query_text(&self) -> String {
        self.query.lines().join("\n")
    }

    /// Append `q` to the in-memory history (most-recent-first), dedupe
    /// against the head so retyping the same query doesn't bloat
    /// history, cap at HISTORY_MAX, and persist. Persistence is
    /// best-effort — any IO error is silently swallowed because losing
    /// history is annoying but not user-blocking.
    fn record_history(&mut self, q: String) {
        if self.history.first().map(String::as_str) == Some(q.as_str()) {
            return;
        }
        // Drop any earlier exact match further down the list — we want
        // a single entry per distinct query, always at the top after
        // most recent use.
        self.history.retain(|h| h != &q);
        self.history.insert(0, q);
        if self.history.len() > HISTORY_MAX {
            self.history.truncate(HISTORY_MAX);
        }
        save_history(&self.history);
    }

    /// Load `history[idx]` into the textarea, saving the current
    /// editor contents into `history_draft` on first entry. Returns
    /// false when `idx` is out of bounds (caller should clamp / no-op).
    fn show_history(&mut self, idx: usize) -> bool {
        if idx >= self.history.len() {
            return false;
        }
        if self.history_idx.is_none() {
            self.history_draft = Some(self.current_query_text());
        }
        self.history_idx = Some(idx);
        let entry = self.history[idx].clone();
        self.replace_query_text(&entry);
        true
    }

    /// Restore the in-flight draft (the text the user had typed before
    /// they started paging history). Called when the user Ctrl-↓'s all
    /// the way back to the bottom — paging history shouldn't destroy
    /// in-progress edits.
    fn exit_history(&mut self) {
        if let Some(draft) = self.history_draft.take() {
            self.replace_query_text(&draft);
        }
        self.history_idx = None;
    }

    /// Replace the textarea contents wholesale and reschedule a compile.
    /// Wraps the existing `replace_query` (used by drill-down) but
    /// skips `schedule_compile` so we don't clear analyze_job — when
    /// the user is paging history we leave any in-flight ANALYZE
    /// alone (it'll get cleared on the *next* compile anyway).
    fn replace_query_text(&mut self, text: &str) {
        self.replace_query(text.to_string());
        self.pending_compile_at = Some(Instant::now() + SQL_THROTTLE);
    }

    /// Kick off an EXPLAIN ANALYZE on a worker thread. Cancels and
    /// replaces any in-flight job (the user pressed `E` again — they
    /// want fresh numbers, not stale ones from a previous query).
    ///
    /// Each invocation gets its own DuckDB connection because:
    /// - duckdb-rs `Connection` isn't Sync (can't borrow from two
    ///   threads simultaneously), so the main TUI loop must keep its
    ///   own connection live for live-preview compiles.
    /// - Spinning up a fresh in-memory connection + httpfs + secrets
    ///   takes ~50 ms — noise compared to the query itself, even for
    ///   trivial files. For remote parquet, opening multiple
    ///   connections lets analyze run concurrently with previews.
    fn start_analyze(&mut self) {
        // Cancel + interrupt any outstanding job. v0.12: we also call
        // interrupt() so the worker's parquet scan unwinds within
        // milliseconds rather than grinding on for tens of seconds
        // against a 40 GB file we no longer care about.
        self.cancel_analyze();
        if self.preview.sql.is_empty() {
            return;
        }
        // Open the worker's connection on the main thread so we can
        // grab its interrupt handle BEFORE moving the connection over.
        // `Connection` is `Send` (duckdb-rs marks it so) so the move
        // is safe; the handle stays here for cancellation.
        let Ok(conn) = crate::open_conn() else {
            return;
        };
        let udfs = self.udfs.clone();
        if crate::register_udfs(&conn, &udfs).is_err() {
            // Macro registration failure is rare (would have caught it
            // on session start). Bail silently — the panel's empty-
            // result path falls back to the main error state.
            return;
        }
        let interrupt = conn.interrupt_handle();
        let (tx, rx) = mpsc::channel();
        let plan_sql = self.preview.sql.clone();
        let file = self.file.clone();
        let query = self.current_query_text();
        thread::spawn(move || {
            if let Some(summary) = run_analyze(&conn, &plan_sql, &file, &query) {
                // tx.send returns Err when the receiver was dropped
                // (user pressed Esc / Ctrl-C to cancel). That's
                // expected — we silently discard the result instead
                // of panicking.
                let _ = tx.send(summary);
            }
            // `conn` is dropped here, freeing the in-memory db.
        });
        self.analyze_job = Some(AnalyzeJob {
            rx,
            started_at: Instant::now(),
            interrupt,
        });
    }

    /// Drop the in-flight ANALYZE job and tell DuckDB to interrupt
    /// the worker's running query.
    ///
    /// v0.12 change: we now call `interrupt()` on the worker's
    /// connection in addition to dropping the receiver. Pre-v0.12 the
    /// receiver-drop alone meant the worker kept grinding through its
    /// scan until DuckDB finished naturally — fine on a 100 MB file,
    /// brutal on a 40 GB one. With interrupt(), the worker returns
    /// from `query()` within milliseconds. The badge clears
    /// immediately either way.
    fn cancel_analyze(&mut self) -> bool {
        if let Some(job) = self.analyze_job.take() {
            job.interrupt.interrupt();
            true
        } else {
            false
        }
    }

    /// Non-blocking check: did the worker thread finish? If so, swap
    /// its summary into `preview.explain`. Called once per run-loop
    /// tick so results show up within ~50 ms of completion.
    fn poll_analyze(&mut self) {
        let Some(job) = &self.analyze_job else { return };
        match job.rx.try_recv() {
            Ok(summary) => {
                self.preview.explain = Some(summary);
                self.analyze_job = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                // Still running — keep showing the badge.
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Worker died without sending (e.g. crate::open_conn
                // failed, or run_analyze returned None on a parser
                // error). Clear the job so the panel header reverts.
                self.analyze_job = None;
            }
        }
    }

    fn equivalent_cli(&self) -> String {
        // v0.11: include `-i ndjson|csv` and `-n N` in the printed one-liner
        // when the TUI was launched with non-default values, so copy-pasting
        // the CLI into a shell reproduces what the user is looking at on
        // screen — not a parquet-only / 20-row-default approximation.
        let q = self.current_query_text();
        let mut parts = vec!["pq".to_string()];
        match self.input_fmt {
            InputFormat::Parquet => {} // default, omit
            InputFormat::Ndjson => parts.extend(["-i".into(), "ndjson".into()]),
            InputFormat::Csv => parts.extend(["-i".into(), "csv".into()]),
        }
        if self.preview_limit != PREVIEW_LIMIT_DEFAULT {
            parts.push("-n".into());
            parts.push(self.preview_limit.to_string());
        }
        for udf in &self.udfs {
            parts.push("--udf".into());
            parts.push(shell_quote(udf));
        }
        parts.push(shell_quote(&self.file));
        if !q.trim().is_empty() {
            parts.push(shell_quote(&q));
        }
        parts.join(" ")
    }

    fn schedule_compile(&mut self) {
        self.pending_compile_at = Some(Instant::now() + SQL_THROTTLE);
        // Any in-flight ANALYZE / preview was scoped to the OLD
        // query — interrupt them so the worker threads don't keep
        // grinding through a multi-GB scan we no longer care about.
        // The preview cancel is what makes typing feel snappy on big
        // remote files: pre-v0.13 every keystroke would queue up a
        // synchronous run_preview that the user couldn't cancel.
        self.cancel_analyze();
        self.cancel_preview();
    }

    /// Tick handler: when the throttle deadline fires, kick off a
    /// new preview on a worker thread. v0.13 — pre-v0.13 this ran
    /// `run_preview` synchronously inside the event loop, blocking
    /// the entire TUI for the duration of the scan.
    fn maybe_run_compile(&mut self) {
        let Some(deadline) = self.pending_compile_at else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.pending_compile_at = None;
        let q = self.current_query_text();
        // Don't burn a worker thread re-running the same query the
        // user already saw results for.
        if q == self.last_compiled && self.preview_job.is_none() {
            return;
        }
        self.start_preview(q);
    }

    /// Start a preview worker for the given query string. Cancels
    /// any preview already in flight (the result we'd get back is
    /// stale relative to what the user just typed).
    fn start_preview(&mut self, query: String) {
        self.cancel_preview();
        // Open the worker's connection on the main thread so we can
        // grab its interrupt handle BEFORE moving the connection
        // into the closure. Connection is `Send` (duckdb-rs marks
        // it so) — the move is safe.
        let Ok(conn) = crate::open_conn() else {
            return;
        };
        let udfs = self.udfs.clone();
        if crate::register_udfs(&conn, &udfs).is_err() {
            // Macro registration failure is rare; fall back to the
            // panel's existing error state (last preview stays).
            return;
        }
        let interrupt = conn.interrupt_handle();
        let (tx, rx) = mpsc::channel();
        let file = self.file.clone();
        let fmt = self.input_fmt;
        let limit = self.preview_limit;
        let q_for_worker = query.clone();
        thread::spawn(move || {
            let preview = run_preview(&conn, &file, &q_for_worker, fmt, limit);
            // Drops the receiver if the user moved on (cancel_preview /
            // schedule_compile dropped this PreviewJob); silently OK.
            let _ = tx.send(preview);
        });
        self.preview_job = Some(PreviewJob {
            rx,
            started_at: Instant::now(),
            interrupt,
            query_when_started: query,
        });
    }

    /// Drop the preview job and tell its DuckDB connection to
    /// interrupt mid-scan. The worker thread will return shortly,
    /// silently discarding its result; we don't wait on it.
    ///
    /// On remote files (`gs://` / `s3://`) DuckDB's HTTPFS only
    /// checks the interrupt flag at chunk boundaries, so a second
    /// Esc / Ctrl-C with no job in flight falls through to quit and
    /// the OS reaps the orphan worker thread on process exit.
    fn cancel_preview(&mut self) -> bool {
        if let Some(job) = self.preview_job.take() {
            let secs = job.started_at.elapsed().as_secs_f32();
            job.interrupt.interrupt();
            self.flash_msg(format!(
                "✋ interrupt sent ({:.1}s) — press again to exit",
                secs
            ));
            true
        } else {
            false
        }
    }

    /// Non-blocking check for a finished preview. When one lands,
    /// promote it to `self.preview` and run the post-result house-
    /// keeping (history record, cursor clamp, derive selected from
    /// query text). Called once per event-loop tick.
    fn poll_preview(&mut self) {
        let Some(job) = &self.preview_job else { return };
        match job.rx.try_recv() {
            Ok(preview) => {
                let q = job.query_when_started.clone();
                self.preview_job = None;
                self.preview = preview;
                self.last_compiled = q.clone();
                if !q.trim().is_empty() {
                    self.record_history(q);
                }
                // Cursor clamp: a smaller projection / wiped filter
                // can leave data_col_idx / data_row_idx pointing past
                // the end of the new result set.
                if let Some(i) = self.data_col_idx {
                    if i >= self.preview.headers.len() {
                        self.data_col_idx = if self.preview.headers.is_empty() {
                            None
                        } else {
                            Some(self.preview.headers.len() - 1)
                        };
                    }
                }
                if let Some(i) = self.data_row_idx {
                    if i >= self.preview.rows.len() {
                        self.data_row_idx = if self.preview.rows.is_empty() {
                            None
                        } else {
                            Some(self.preview.rows.len() - 1)
                        };
                    }
                }
                // Re-derive column.selected from the just-compiled
                // query text (string-match) so the Columns panel
                // stays in sync with the projection.
                let q_lower = self.last_compiled.to_ascii_lowercase();
                for c in &mut self.columns {
                    let needle = format!(".{}", c.name.to_ascii_lowercase());
                    c.selected = q_lower.contains(&needle);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                // Still running.
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Worker died without sending — clear so the badge
                // doesn't render forever.
                self.preview_job = None;
            }
        }
    }

    fn toggle_current_column(&mut self) {
        let Some(idx) = self.column_state.selected() else {
            return;
        };
        let Some(col) = self.columns.get(idx) else {
            return;
        };
        let token = format!(".{}", col.name);
        let cur = self.current_query_text();

        let new_q = if cur.trim().is_empty() {
            // Bare projection of just this column.
            token
        } else if col.selected {
            // Try to remove the token. Naive: drop "  .col, " or ".col" segments.
            // Good enough for MVP — full projection editing comes in v0.6.
            let stripped = cur
                .replace(&format!(", {token}"), "")
                .replace(&format!("{token}, "), "")
                .replace(&token, "")
                .trim_end_matches(", ")
                .to_string();
            stripped
        } else {
            // Insert into existing projection. If query starts with `.<col>`
            // pattern, append `, .col`; otherwise prepend a new projection stage.
            if cur.trim_start().starts_with('.') {
                format!("{cur}, {token}")
            } else {
                format!("{token} | {cur}")
            }
        };

        self.replace_query(new_q);
        self.schedule_compile();
    }

    /// Append the current column to the projection. Unlike `toggle_*`, this
    /// is purely additive — Enter on a column that's already projected is a
    /// no-op (rather than removing it). Useful when you're building up a
    /// `.a, .b, .c` shape and don't want to accidentally delete things.
    fn append_current_column(&mut self) {
        let Some(idx) = self.column_state.selected() else {
            return;
        };
        let Some(col) = self.columns.get(idx) else {
            return;
        };
        if col.selected {
            self.flash_msg(format!(".{} already in projection", col.name));
            return;
        }
        let token = format!(".{}", col.name);
        let cur = self.current_query_text();
        let new_q = if cur.trim().is_empty() {
            token
        } else if cur.trim_start().starts_with('.') {
            format!("{cur}, {token}")
        } else {
            format!("{token} | {cur}")
        };
        self.replace_query(new_q);
        self.schedule_compile();
    }

    fn replace_query(&mut self, text: String) {
        let mut new_ta = TextArea::default();
        // Keep the same ghost-text placeholder + styling that App::new
        // sets up — without this, replacing with empty text would lose
        // the "(.col1, .col2 where … | group_by …)" hint.
        new_ta.set_placeholder_text(QUERY_PLACEHOLDER);
        new_ta.set_placeholder_style(Style::default().fg(Color::DarkGray));
        new_ta.insert_str(&text);
        new_ta.set_block(Block::default().borders(Borders::ALL).title(" Query "));
        self.query = new_ta;
    }

    fn copy_cli_to_clipboard(&mut self) {
        // arboard is optional — not all systems have a working clipboard
        // (headless Linux containers, minimal Docker images, etc.). We try,
        // fall back to printing the CLI to the flash bar so the user can
        // hand-copy from the screen.
        let cli = self.equivalent_cli();
        // arboard isn't a hard dep yet — for the MVP we stash to flash only.
        // v0.6 wires up arboard properly with a feature flag.
        self.flash = Some((
            format!("Y → CLI: {cli}  (arboard wiring lands in v0.6)"),
            Instant::now(),
        ));
    }

    fn flash_msg(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    // ── v0.6 semantic sync helpers ─────────────────────────────────────────
    //
    /// Recompute the lineage from the current query buffer. Cheap (single
    /// pass over the string) — fine to call on every keystroke.
    fn refresh_lineage(&mut self) {
        self.lineage = lineage::extract(&self.current_query_text());
    }

    /// The "active" source column for cross-panel highlighting. Resolved
    /// from focus + cursor:
    ///
    ///   - Query   → byte offset of textarea cursor → lineage.column_at()
    ///   - Data    → header at data_col_idx → lineage.source_of() (derived
    ///     aliases like sum_revenue resolve to revenue) or the header
    ///     itself (plain projections).
    ///   - Columns → the row currently selected in the Columns list.
    ///
    /// Returns None when there's nothing meaningful to highlight (e.g.
    /// cursor parked in whitespace, no rows yet).
    fn active_source(&self) -> Option<String> {
        match self.focus {
            Panel::Query => {
                let off = cursor_byte_offset(&self.query);
                self.lineage.column_at(off).map(|r| r.name.clone())
            }
            Panel::Data => {
                let idx = self.data_col_idx?;
                let header = self.preview.headers.get(idx)?;
                if let Some(src) = self.lineage.source_of(header) {
                    Some(src.to_string())
                } else {
                    Some(header.clone())
                }
            }
            Panel::Columns => {
                let idx = self.column_state.selected()?;
                self.columns.get(idx).map(|c| c.name.clone())
            }
        }
    }

    /// Recompute the schema-completion popup state from the cursor position.
    /// Called after every textarea keystroke. Sets `self.completion` to
    /// `Some(_)` iff:
    ///   - cursor is positioned just after one or more identifier chars
    ///   - those chars are immediately preceded by a `.`
    ///   - at least one schema column matches the typed prefix
    ///
    /// We don't fire when the prefix is empty (just typed `.`) on purpose —
    /// 99% of the time the user wants to see candidates only after they've
    /// committed at least one letter, otherwise the popup pops up on every
    /// projection comma and gets in the way.
    fn refresh_completion(&mut self) {
        self.completion = compute_completion(&self.query, &self.columns);
    }

    /// Drill-down: take the currently-selected row in the Data panel and
    /// rewrite the query to filter on its grouping-column values. Replaces
    /// the entire query buffer (rather than appending) — the user's mental
    /// model is "from this aggregate, show me the underlying rows", which
    /// only makes sense without the original group_by/aggregates.
    ///
    /// The previous query is stashed in `drill_undo` so Backspace in the
    /// Data panel can pop back out.
    ///
    /// Flashes a status message and bails when:
    ///   - no row is selected (user pressed Enter before navigating with ↑/↓)
    ///   - the query has no aggregate columns (no group_by → already raw rows)
    fn drill_down(&mut self) {
        let Some(row_idx) = self.data_row_idx else {
            self.flash_msg("drill-down: pick a row with ↑/↓ first");
            return;
        };
        let Some(row) = self.preview.rows.get(row_idx) else {
            return;
        };
        let drill = match build_drill_query(&self.preview.headers, row, &self.lineage) {
            Some(q) => q,
            None => {
                self.flash_msg("drill-down needs group_by — try `group_by .col | count`");
                return;
            }
        };
        let cur = self.current_query_text();
        self.drill_undo = Some(cur);
        self.replace_query(drill);
        self.refresh_lineage();
        self.refresh_completion();
        self.schedule_compile();
        // Reset cursors so the new (raw, ungrouped) result set isn't viewed
        // through the lens of the prior aggregate's column highlights.
        self.data_col_idx = None;
        self.data_row_idx = None;
        self.flash_msg("drilled in — Backspace to undo");
    }

    /// Pop the most recent drill-down. Single-level — successive
    /// Backspaces are a no-op once the stash is consumed.
    fn drill_undo(&mut self) {
        let Some(prev) = self.drill_undo.take() else {
            self.flash_msg("nothing to undo");
            return;
        };
        self.replace_query(prev);
        self.refresh_lineage();
        self.refresh_completion();
        self.schedule_compile();
        self.flash_msg("drill-down undone");
    }

    /// Insert the currently-selected candidate into the query buffer,
    /// replacing the `.prefix` span. No-op when no completion is active.
    fn accept_completion(&mut self) {
        let Some(c) = self.completion.take() else {
            return;
        };
        let Some(pick) = c.candidates.get(c.selected).cloned() else {
            return;
        };
        let full = self.current_query_text();
        let span_end = c.dot_byte + 1 + c.prefix.len();
        // Defensive bounds check — `compute_completion` only emits valid spans
        // for the buffer it observed, but the buffer might shift between calls.
        if span_end > full.len() {
            return;
        }
        let mut rebuilt = String::with_capacity(full.len() + pick.len());
        rebuilt.push_str(&full[..c.dot_byte]);
        rebuilt.push('.');
        rebuilt.push_str(&pick);
        rebuilt.push_str(&full[span_end..]);
        self.replace_query(rebuilt);
        // Move cursor to right after the inserted name. We do this by
        // replacing the textarea (set up by replace_query) and then
        // forward-walking the cursor — tui-textarea has no direct
        // "set cursor at byte offset", but we can move down/right one
        // line at a time. For a single-line query (the common case) the
        // cursor naturally lands at end-of-line which is what we want.
        // Multi-line queries are rare in the TUI; correctness > polish.
        self.refresh_lineage();
        self.schedule_compile();
    }
}

// ─── Event handling ──────────────────────────────────────────────────────────

fn on_key(app: &mut App<'_>, key: KeyEvent) {
    // Ctrl-C: prefer to interrupt a running query first; only quit
    // if there's nothing to cancel. v0.13 — preview is now async, so
    // Ctrl-C cancels it too. Order: preview > analyze > quit. The
    // distinction matters when both are running (preview kicked off
    // by typing, analyze kicked off by E): Ctrl-C kills the cheaper
    // / more recent thing first so the user can iterate quickly.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.cancel_preview() || app.cancel_analyze() {
            return;
        }
        app.should_quit = true;
        return;
    }
    // Help overlay is modal: any key dismisses it. We swallow the keypress
    // (don't pass it through to the panel below) so users can press `?` to
    // peek and another arbitrary key to dismiss without surprise side
    // effects (e.g. accidentally toggling a column).
    if app.show_help {
        app.show_help = false;
        return;
    }
    // ── Esc cancels an in-flight ANALYZE before anything else. ────────
    // Without this, Esc in Columns/Data would quit the TUI even though
    // the user almost certainly meant "stop the slow analyze, I changed
    // my mind". Esc in the Query panel still falls through to its
    // normal "drop focus" behavior on the second press. We don't
    // shortcut Tab/BackTab so the user can keep navigating panels even
    // while the worker thread is alive in the background.
    // v0.13 — Esc cancels in-flight preview / analyze before doing
    // anything panel-specific. Same priority as Ctrl-C: preview >
    // analyze > fall through to per-panel handling.
    if key.code == KeyCode::Esc && (app.cancel_preview() || app.cancel_analyze()) {
        return;
    }
    if key.code == KeyCode::Tab {
        app.focus = app.focus.next();
        return;
    }
    if key.code == KeyCode::BackTab {
        app.focus = app.focus.prev();
        return;
    }
    // Panel-specific keys.
    match app.focus {
        Panel::Columns => on_key_columns(app, key),
        Panel::Query => on_key_query(app, key),
        Panel::Data => on_key_data(app, key),
    }
}

fn on_key_columns(app: &mut App<'_>, key: KeyEvent) {
    match key.code {
        // Esc anywhere outside the Query panel is "I'm done, leave". One Esc
        // from inside Query goes back to Columns first; then a second Esc
        // exits — vim/lazygit/ranger pattern.
        KeyCode::Esc | KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('Q') => {
            app.should_quit = true;
            app.print_cli_on_exit = true;
        }
        KeyCode::Char('Y') => app.copy_cli_to_clipboard(),
        KeyCode::Char(':') => app.show_sql = !app.show_sql,
        KeyCode::Char('e') => app.show_explain = !app.show_explain,
        KeyCode::Char('E') => {
            app.show_explain = true;
            app.start_analyze();
        }
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Enter => app.append_current_column(),
        KeyCode::Down | KeyCode::Char('j') => {
            let len = app.columns.len();
            if len == 0 {
                return;
            }
            let i = app.column_state.selected().unwrap_or(0);
            app.column_state.select(Some((i + 1).min(len - 1)));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.column_state.selected().unwrap_or(0);
            app.column_state.select(Some(i.saturating_sub(1)));
        }
        KeyCode::Char(' ') => app.toggle_current_column(),
        _ => {}
    }
}

fn on_key_query(app: &mut App<'_>, key: KeyEvent) {
    // ── Completion popup: intercept nav keys when popup is open. ──────────
    //
    // We dispatch this BEFORE the textarea hatches so that Up/Down inside
    // an active popup never accidentally rolls the textarea cursor instead.
    // Esc dismisses the popup but stays in the Query panel — pressing Esc
    // again then drops back to Columns (the standard one-Esc-per-level
    // pattern lazygit/vim users expect).
    if app.completion.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.completion = None;
                return;
            }
            KeyCode::Up => {
                if let Some(c) = app.completion.as_mut() {
                    if c.selected > 0 {
                        c.selected -= 1;
                    }
                }
                return;
            }
            KeyCode::Down => {
                if let Some(c) = app.completion.as_mut() {
                    if c.selected + 1 < c.candidates.len() {
                        c.selected += 1;
                    }
                }
                return;
            }
            KeyCode::Tab | KeyCode::Enter => {
                app.accept_completion();
                return;
            }
            _ => {} // fall through — let the textarea handle the keystroke
        }
    }

    // In Query panel, most keys go to the textarea. Carve out a few escape
    // hatches (Esc to leave focus, Ctrl-Y to copy CLI, Ctrl-Q to quit).
    if key.code == KeyCode::Esc {
        app.focus = Panel::Columns;
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q') => {
                app.should_quit = true;
                app.print_cli_on_exit = true;
                return;
            }
            KeyCode::Char('y') => {
                app.copy_cli_to_clipboard();
                return;
            }
            // ── Query history navigation (v0.8) ─────────────────────────
            // We use Ctrl-Up/Down rather than bare Up/Down so the
            // textarea's intra-buffer arrow-key navigation still
            // works for multi-line queries. The trade-off: discoverable
            // only via `?`. Documented in the help overlay.
            KeyCode::Up => {
                let next = app.history_idx.map_or(0, |i| i + 1);
                if app.show_history(next) {
                    app.refresh_lineage();
                    app.refresh_completion();
                }
                return;
            }
            KeyCode::Down => match app.history_idx {
                Some(0) => {
                    app.exit_history();
                    app.refresh_lineage();
                    app.refresh_completion();
                }
                Some(i) => {
                    app.show_history(i - 1);
                    app.refresh_lineage();
                    app.refresh_completion();
                }
                None => {} // Already at "live" position; nothing to do.
            },
            _ => {}
        }
    }
    // Forward to the textarea.
    let consumed = app.query.input(key);
    if consumed {
        app.schedule_compile();
        // Once the user starts editing, they're no longer browsing
        // history — the draft they had stashed is now this newly-
        // edited buffer. Clear the paging cursor so the next Ctrl-↑
        // starts fresh at index 0 (most-recent), not from wherever
        // they happened to land while browsing.
        if app.history_idx.is_some() {
            app.history_idx = None;
            app.history_draft = None;
        }
    }
    // Lineage and completion are derived state — refresh after every event,
    // including pure cursor-moves (Left/Right/Home/End/...) which don't
    // schedule a compile but do change which token the cursor sits on.
    app.refresh_lineage();
    app.refresh_completion();
}

fn on_key_data(app: &mut App<'_>, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.should_quit = true,
        // 'q' as quit only when no row-cursor is set — once the user is
        // actively driving the row picker (typing j/k), bare `q` would be
        // a surprising quit. They can still use Esc or Q.
        KeyCode::Char('q') if app.data_row_idx.is_none() => app.should_quit = true,
        KeyCode::Char('Q') => {
            app.should_quit = true;
            app.print_cli_on_exit = true;
        }
        KeyCode::Char('Y') => app.copy_cli_to_clipboard(),
        KeyCode::Char(':') => app.show_sql = !app.show_sql,
        KeyCode::Char('e') => app.show_explain = !app.show_explain,
        KeyCode::Char('E') => {
            app.show_explain = true;
            app.start_analyze();
        }
        KeyCode::Char('?') => app.show_help = true,
        // ── v0.6: column-cursor navigation. ───────────────────────────────
        //
        // Move data_col_idx left/right to highlight a specific column.
        // Active column drives semantic sync into the Columns panel —
        // landing on `sum_revenue` highlights `revenue`, landing on `country`
        // highlights `country` itself.
        KeyCode::Right | KeyCode::Char('l') => {
            let n = app.preview.headers.len();
            if n == 0 {
                return;
            }
            app.data_col_idx = Some(match app.data_col_idx {
                Some(i) if i + 1 < n => i + 1,
                Some(i) => i,
                None => 0,
            });
        }
        KeyCode::Left | KeyCode::Char('h') => {
            let n = app.preview.headers.len();
            if n == 0 {
                return;
            }
            app.data_col_idx = Some(match app.data_col_idx {
                Some(0) | None => 0,
                Some(i) => i - 1,
            });
        }
        // ── v0.6: row-cursor navigation + drill-down. ─────────────────────
        //
        // ↑/↓ (and j/k) seed/move the row cursor. Enter on a selected row
        // rewrites the query into a `where` clause filtering on every
        // grouping-column value. Backspace pops back to the pre-drill query.
        KeyCode::Down | KeyCode::Char('j') => {
            let n = app.preview.rows.len();
            if n == 0 {
                return;
            }
            app.data_row_idx = Some(match app.data_row_idx {
                Some(i) if i + 1 < n => i + 1,
                Some(i) => i,
                None => 0,
            });
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let n = app.preview.rows.len();
            if n == 0 {
                return;
            }
            app.data_row_idx = Some(match app.data_row_idx {
                Some(0) | None => 0,
                Some(i) => i - 1,
            });
        }
        KeyCode::Enter => app.drill_down(),
        KeyCode::Backspace => app.drill_undo(),
        _ => {}
    }
}

// ─── Rendering ───────────────────────────────────────────────────────────────

fn render(f: &mut ratatui::Frame, app: &mut App<'_>) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(40)])
        .split(outer[0]);

    // Left column: Columns (top, takes most of it) + Filters (bottom 7 rows).
    // Filters is a *display-only* panel for now — it shows where/having
    // clauses extracted from the current query so users can see at a glance
    // which filters are active, even when their query has folded into a
    // single line. Editing happens in the Query panel.
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(7)])
        .split(body[0]);
    render_columns(f, app, left[0]);
    render_filters(f, app, left[1]);

    // Right column layout grows dynamically based on which optional
    // panels are toggled on. Order top-to-bottom: Query (always), SQL
    // (`:`), Data (always), Explain (`e`). We keep Data at Min(5) so it
    // gets the leftover space; the optional panels each take a fixed
    // small slice. Indices into `right` are computed alongside the
    // constraint vec so the dispatch below can't drift out of sync.
    let mut right_constraints: Vec<Constraint> = Vec::with_capacity(4);
    let query_idx = right_constraints.len();
    right_constraints.push(Constraint::Length(6));
    let sql_idx = if app.show_sql {
        let i = right_constraints.len();
        right_constraints.push(Constraint::Length(6));
        Some(i)
    } else {
        None
    };
    let data_idx = right_constraints.len();
    right_constraints.push(Constraint::Min(5));
    let explain_idx = if app.show_explain {
        let i = right_constraints.len();
        right_constraints.push(Constraint::Length(8));
        Some(i)
    } else {
        None
    };
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(right_constraints)
        .split(body[1]);

    let query_area = right[query_idx];
    render_query(f, app, query_area);
    if let Some(i) = sql_idx {
        render_sql(f, app, right[i]);
    }
    render_data(f, app, right[data_idx]);
    if let Some(i) = explain_idx {
        render_explain(f, app, right[i]);
    }

    render_status_bar(f, app, outer[1]);

    // ── v0.6: schema completion popup ─────────────────────────────────────
    //
    // Drawn above the data table but below the help overlay. We anchor it
    // to the textarea cursor position so it visually "drops down" from
    // where the user is typing — same pattern as VS Code / IntelliJ.
    // Rendered only when the popup is active.
    if app.completion.is_some() {
        render_completion(f, app, query_area);
    }

    // Help overlay rendered last so it sits on top of every panel.
    if app.show_help {
        render_help(f, f.area());
    }
}

fn focused_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_columns(f: &mut ratatui::Frame, app: &mut App<'_>, area: Rect) {
    let active = app.focus == Panel::Columns;
    // The "active source" comes from whichever panel currently owns focus —
    // see App::active_source. When the Columns panel is focused this is
    // just the row the user is hovering; the highlight is most interesting
    // when focus is in *another* panel (Query/Data) and the user can see
    // their cursor's lineage land here without leaving the editor.
    let active_src = app.active_source();
    // Style for the lineage-linked row.
    //
    // We deliberately use 24-bit RGB gold (255,215,0) instead of the named
    // `Color::Yellow`: the latter resolves to ANSI 16-color slot 3, which
    // most terminal palettes render as a muted brown that's nearly
    // indistinguishable from `Modifier::BOLD` on a default white fg. The
    // RGB form survives any palette and reads as unmistakably yellow.
    let star_style = Style::default()
        .fg(Color::Rgb(255, 215, 0))
        .add_modifier(Modifier::BOLD);

    let items: Vec<ListItem> = app
        .columns
        .iter()
        .map(|c| {
            let is_active = active_src.as_deref() == Some(c.name.as_str());
            // Two-char prefix: ★ when this row is the active lineage target,
            // otherwise the existing ✓ / blank for projection membership.
            // Keeping it two chars wide means the column names stay
            // vertically aligned across all rows.
            let mark = if is_active {
                "★ "
            } else if c.selected {
                "✓ "
            } else {
                "  "
            };
            let name_style = if is_active {
                star_style
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let mark_style = if is_active {
                star_style
            } else {
                Style::default().fg(Color::Green)
            };
            let line = Line::from(vec![
                Span::styled(mark, mark_style),
                Span::styled(&c.name, name_style),
                Span::raw("  "),
                Span::styled(&c.ty, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    // Title surfaces the input format whenever it isn't the default
    // (parquet) — useful confirmation when the user opened a `.ndjson`
    // / `.csv` file or passed `-i` explicitly. Hidden for parquet to
    // keep the title compact in the common case.
    let fmt_tag = match app.input_fmt {
        InputFormat::Parquet => "",
        InputFormat::Ndjson => " · ndjson",
        InputFormat::Csv => " · csv",
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focused_style(active))
                .title(format!(" Columns · {}{} ", app.columns.len(), fmt_tag)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.column_state);
}

fn render_query(f: &mut ratatui::Frame, app: &mut App<'_>, area: Rect) {
    let active = app.focus == Panel::Query;
    // v0.13: when the preview worker is running, show "running 1.2s"
    // in the header. Without this badge a slow remote scan would
    // look indistinguishable from "TUI hung" — even though the
    // event loop is still ticking and the result will land
    // asynchronously. Badge takes priority over the previous
    // result's last_ms / error so the user knows which numbers are
    // fresh.
    let title = if let Some(job) = &app.preview_job {
        let secs = job.started_at.elapsed().as_secs_f32();
        format!(" Query · running {:.1}s · Esc/Ctrl-C cancels ", secs)
    } else if let Some(err) = &app.preview.error {
        format!(" Query · ⚠ {} ", err.lines().next().unwrap_or(""))
    } else {
        format!(" Query · {} ms ", app.preview.last_ms)
    };
    app.query.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(focused_style(active))
            .title(title),
    );

    // ── v0.6: highlight every reference to the active source column ──────
    //
    // tui-textarea exposes a builtin search facility — we abuse it here as
    // a "highlight all matches" channel. Pattern is `\.<name>\b` so we
    // catch `.country` but not `.country_code`, and we don't accidentally
    // match a literal substring of an unrelated identifier. Empty pattern
    // clears the highlight.
    let highlight_pattern = match app.active_source() {
        Some(name) if !name.is_empty() => format!(r"\.{}\b", regex_escape(&name)),
        _ => String::new(),
    };
    if let Err(e) = app.query.set_search_pattern(&highlight_pattern) {
        // tui-textarea returns an error only on invalid regex — our pattern
        // is escaped so this should never trigger. Stuff a flash for the
        // unlikely case someone names a column with a regex metachar
        // we missed.
        app.flash_msg(format!("highlight pattern err: {e}"));
    }
    // Same RGB-vs-named-color rationale as the Columns ★ row — `Color::Yellow`
    // renders muted on most palettes. We pair a deep-olive bg with a bright
    // gold fg so matched references read as a "highlighter swipe" even when
    // the user's cursor parks on top of one (the cursor's own yellow bg sits
    // above this and still wins by being noticeably brighter).
    app.query.set_search_style(
        Style::default()
            .bg(Color::Rgb(120, 100, 0))
            .fg(Color::Rgb(255, 215, 0))
            .add_modifier(Modifier::BOLD),
    );

    // Cursor styling reflects focus. tui-textarea draws the cursor itself
    // (we never call f.set_cursor_position) — without an explicit style the
    // default REVERSED modifier collapses to invisible against the
    // placeholder's DarkGray fg, which is the bug we hit.
    if active {
        app.query.set_cursor_style(
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        );
        app.query
            .set_cursor_line_style(Style::default().bg(Color::Rgb(40, 40, 40)));
    } else {
        // Unfocused: hide the cursor entirely so it's not stealing
        // attention from whatever panel the user is actually driving.
        app.query.set_cursor_style(Style::default());
        app.query.set_cursor_line_style(Style::default());
    }
    f.render_widget(&app.query, area);
}

/// Minimal regex escaper for column names. tui-textarea's search uses the
/// `regex` crate, so we escape any byte that's a metachar. We don't pull in
/// regex::escape because that crate isn't a direct dep yet — replicating the
/// 14 escape chars by hand is trivial and avoids a transitive surprise.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn render_sql(f: &mut ratatui::Frame, app: &App<'_>, area: Rect) {
    let p = Paragraph::new(app.preview.sql.clone()).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Compiled SQL · press : to collapse "),
    );
    f.render_widget(p, area);
}

fn render_data(f: &mut ratatui::Frame, app: &App<'_>, area: Rect) {
    let active = app.focus == Panel::Data;
    // Title hint: report `cap=N` whenever the row count hits the LIMIT
    // we injected — that's the user's signal that more data exists and
    // they can pass `pq tui -n 200 …` to widen the window. We deliberately
    // don't compute true row counts here because that would force a
    // second SELECT count(*) per keystroke on remote files.
    let cap_hint = if app.preview.rows.len() >= app.preview_limit {
        format!(" (cap={})", app.preview_limit)
    } else {
        String::new()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focused_style(active))
        .title(format!(
            " Data · {} rows{} ",
            app.preview.rows.len(),
            cap_hint
        ));

    if app.preview.headers.is_empty() {
        let msg = match &app.preview.error {
            Some(e) => format!("error:\n  {e}"),
            None => "(no rows)".to_string(),
        };
        let p = Paragraph::new(msg).block(block);
        f.render_widget(p, area);
        return;
    }

    // Column widths: max(header, max cell), capped at 40. Anything longer just
    // gets ellipsised by the cell renderer — we'd rather see all columns than
    // dedicate the whole viewport to one wide string. v0.6: horizontal scroll.
    const MIN_W: u16 = 4;
    const MAX_W: u16 = 40;
    let ncols = app.preview.headers.len();
    let mut col_widths: Vec<u16> = app
        .preview
        .headers
        .iter()
        .map(|h| (display_width(h) as u16).clamp(MIN_W, MAX_W))
        .collect();
    for row in &app.preview.rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            let w = display_width(cell) as u16;
            col_widths[i] = col_widths[i].max(w.min(MAX_W));
        }
    }

    // Heuristic numeric detection — right-align if every non-empty value in
    // the column parses as a number-ish thing (digits, ., -, +, e, E, ,, ∅).
    let numeric: Vec<bool> = (0..ncols)
        .map(|i| {
            let mut any = false;
            for r in &app.preview.rows {
                if let Some(s) = r.get(i) {
                    if s.is_empty() || s == "∅" {
                        continue;
                    }
                    any = true;
                    if !looks_numeric(s) {
                        return false;
                    }
                }
            }
            any
        })
        .collect();

    // ── v0.6: which column should glow? ──────────────────────────────────
    //
    // Two highlight inputs combine here:
    //   1. data_col_idx: explicit column-cursor (Data panel has focus, user
    //      navigated with ←/→). Always wins when set.
    //   2. active_source: lineage from another panel. When the Query cursor
    //      sits on `.revenue`, the Data panel should highlight `revenue` AND
    //      any derived column produced *from* revenue (sum_revenue, etc.).
    //
    // We compute one set of "lit" column indices and apply the same style
    // to both header cell and body cells of those columns.
    let active_src = app.active_source();
    let mut lit: Vec<bool> = vec![false; ncols];
    if let Some(idx) = app.data_col_idx {
        if idx < ncols {
            lit[idx] = true;
        }
    }
    if let Some(src) = active_src.as_deref() {
        for (i, h) in app.preview.headers.iter().enumerate() {
            // Direct match: header IS the source column.
            if h == src {
                lit[i] = true;
                continue;
            }
            // Derived match: header is an alias whose source column matches.
            if app.lineage.source_of(h) == Some(src) {
                lit[i] = true;
            }
        }
    }
    let lit_header_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let lit_cell_style = Style::default().bg(Color::Rgb(60, 50, 0)); // muted gold

    let header = Row::new(app.preview.headers.iter().enumerate().map(|(i, h)| {
        let style = if lit[i] {
            lit_header_style
        } else {
            let mut s = Style::default().add_modifier(Modifier::BOLD);
            if numeric[i] {
                s = s.fg(Color::Cyan);
            }
            s
        };
        Cell::from(h.clone()).style(style)
    }));

    // Row highlight: the row at data_row_idx (if any) gets a subtle bg so
    // users can see which row Enter would drill on. Distinct enough from
    // the column-cursor gold tint that the two compose at the intersection
    // (column + row both highlight → cell is the brightest).
    let row_lit_style = Style::default().bg(Color::Rgb(40, 40, 60));
    let rows: Vec<Row> = app
        .preview
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, r)| {
            let row_is_picked = app.data_row_idx == Some(row_idx);
            let r = Row::new(r.iter().enumerate().map(|(i, c)| {
                let w = col_widths[i] as usize;
                let truncated = ellipsise(c, w);
                let text = if numeric.get(i).copied().unwrap_or(false) {
                    // Right-align numerics by padding on the left to col width.
                    format!("{:>w$}", truncated, w = w)
                } else {
                    truncated
                };
                let cell = Cell::from(text);
                if lit[i] {
                    cell.style(lit_cell_style)
                } else {
                    cell
                }
            }));
            if row_is_picked {
                r.style(row_lit_style)
            } else {
                r
            }
        })
        .collect();

    let widths: Vec<Constraint> = col_widths.iter().map(|w| Constraint::Length(*w)).collect();
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(2);
    f.render_widget(table, area);
}

/// Display width — counts chars, not bytes. Cheap stand-in for unicode-width;
/// good enough for ASCII / common text. Wide CJK glyphs will under-count by
/// up to half, but the MAX_W cap stops anything from being unreadable.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

/// Truncate `s` to fit in `max_chars` columns, replacing the last char with
/// `…` so users can tell at a glance that the cell was clipped (vs. a value
/// that happens to look short). No-op when the value already fits.
fn ellipsise(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

fn looks_numeric(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let mut seen_digit = false;
    for c in t.chars() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' | '-' | '+' | ',' | 'e' | 'E' | '_' => {}
            _ => return false,
        }
    }
    seen_digit
}

fn render_status_bar(f: &mut ratatui::Frame, app: &App<'_>, area: Rect) {
    // Status bar swaps between three contexts:
    //   1. flash message (e.g. "Y → CLI: …") — wins for 3s
    //   2. completion popup help — when popup is open
    //   3. default panel-key cheatsheet — otherwise
    //
    // The completion-mode hint exists because the regular bar's keys
    // (Tab, Enter, ←/→) get rebound while the popup is up; surfacing the
    // override avoids the "why didn't Enter quit?" confusion.
    let default_help = " Tab next │ ␣ toggle │ ⏎ append/drill │ ⌫ undo │ Q exit+print │ Esc quit │ : SQL │ e Explain │ E ANALYZE │ ? help ";
    let completion_help = " ↑↓ pick · ⏎/Tab insert · Esc cancel · keep typing to filter ";
    let text = match (&app.flash, app.completion.is_some()) {
        (Some((msg, _)), _) => msg.clone(),
        (None, true) => completion_help.to_string(),
        (None, false) => default_help.to_string(),
    };
    let p = Paragraph::new(text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(p, area);
}

fn render_filters(f: &mut ratatui::Frame, app: &App<'_>, area: Rect) {
    let filters = extract_filters(&app.last_compiled);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(format!(" Filters · {} ", filters.len()));

    if filters.is_empty() {
        let p = Paragraph::new(Span::styled(
            "(none — type `where .col == …` in Query)",
            Style::default().fg(Color::DarkGray),
        ))
        .block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = filters
        .into_iter()
        .map(|f| {
            ListItem::new(Line::from(vec![
                Span::styled("• ", Style::default().fg(Color::Yellow)),
                Span::raw(f),
            ]))
        })
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

/// Extract `where … | …` and `having …` stages from a pq DSL query string,
/// for the read-only Filters panel. Lossy by design — we don't try to
/// reconstruct the full parser; just pull each filter expression out with
/// enough fidelity that users see what conditions are active.
///
/// Tolerant of mid-typing buffers: when the user has just typed the keyword
/// itself (e.g. `.country, .revenue where`) and nothing follows, we emit no
/// filter rather than panicking on an out-of-bounds slice.
fn extract_filters(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    for stage in q.split('|').map(str::trim) {
        if stage.is_empty() {
            continue;
        }
        for kw in &["where", "having"] {
            if let Some(pos) = find_word(stage, kw) {
                // Slice past the keyword itself. `pos + kw.len()` lands one
                // byte past the keyword's last char — always a valid index
                // because `find_word` returned it. Note: don't reuse the
                // old `kw + " "` length here — that crashed when the user
                // typed a bare `where` with no trailing space.
                let after_kw = pos + kw.len();
                let expr = stage
                    .get(after_kw..)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string();
                if !expr.is_empty() {
                    let prefix = if *kw == "having" { "(having) " } else { "" };
                    out.push(format!("{prefix}{expr}"));
                }
                break;
            }
        }
    }
    out
}

/// Word-bounded, case-insensitive substring find. Returns the byte offset
/// at which `needle` appears in `haystack`, requiring whitespace (or start
/// of string) immediately before. Avoids matching `whereabouts` as `where`.
fn find_word(haystack: &str, needle: &str) -> Option<usize> {
    let lower = haystack.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut start = 0usize;
    while let Some(rel) = lower[start..].find(&needle_lower) {
        let abs = start + rel;
        let prev_ok = abs == 0 || lower.as_bytes()[abs - 1].is_ascii_whitespace();
        let after = abs + needle_lower.len();
        let after_ok = after >= lower.len() || lower.as_bytes()[after].is_ascii_whitespace();
        if prev_ok && after_ok {
            return Some(abs);
        }
        start = abs + 1;
    }
    None
}

fn render_explain(f: &mut ratatui::Frame, app: &App<'_>, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let ok = Style::default().fg(Color::Green);
    let warn = Style::default().fg(Color::Rgb(255, 215, 0)); // gold (matches ★)
    let analyzed_color = Style::default().fg(Color::Rgb(0, 200, 220)); // teal accent

    // Title varies by state: in-flight ANALYZE (with live elapsed
    // ticker), post-ANALYZE summary, or plain EXPLAIN. Keep all three
    // distinct so the user always knows whether they're looking at
    // estimates, actuals, or a query that's still computing.
    let title = if let Some(job) = &app.analyze_job {
        let elapsed = job.started_at.elapsed();
        let stamp = if elapsed.as_secs() < 1 {
            format!("{:.1}s", elapsed.as_secs_f64())
        } else {
            format!("{:.0}s", elapsed.as_secs_f64())
        };
        format!(" Explain · ANALYZE running… {stamp} — Esc to cancel ")
    } else {
        match &app.preview.explain {
            Some(e) if e.analyzed => match e.total_seconds {
                Some(s) if s < 1.0 => format!(" Explain · ANALYZE  {:.1} ms ", s * 1000.0),
                Some(s) => format!(" Explain · ANALYZE  {:.2} s ", s),
                None => " Explain · ANALYZE ".to_string(),
            },
            _ => " Explain · `e` toggle / `E` ANALYZE ".to_string(),
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);

    let lines: Vec<Line> = match &app.preview.explain {
        // No explain at all — usually means the SQL itself errored, in
        // which case the Query panel header already shows the parse/
        // binder message; we just point users there.
        None => vec![Line::from(Span::styled(
            "(query failed — fix it to see the plan)",
            dim,
        ))],
        Some(e) => {
            let total_filters: usize = e.scans.iter().map(|s| s.filters.len()).sum();
            let total_proj: usize = e.scans.iter().map(|s| s.projections.len()).sum();
            let est_total: u64 = e.scans.iter().filter_map(|s| s.estimated_rows).sum();
            let act_total: u64 = e.scans.iter().filter_map(|s| s.actual_rows).sum();

            let mut out = Vec::new();

            // First line: at-a-glance summary. ANALYZE swaps in actual
            // rows + a teal "ACTUAL" label so the user can tell at a
            // glance which numbers are real measurements vs estimates.
            let mut summary: Vec<Span> = vec![
                Span::raw(format!("{} scan(s)", e.scans.len())),
                Span::raw("  •  "),
            ];
            if e.analyzed && act_total > 0 {
                summary.push(Span::styled(
                    format!("{} actual rows", fmt_count(act_total)),
                    analyzed_color,
                ));
            } else {
                summary.push(Span::raw(format!(
                    "~{} estimated rows",
                    fmt_count(est_total)
                )));
            }
            summary.push(Span::raw("  •  "));
            summary.push(Span::raw(format!("{} filter(s) pushed", total_filters)));
            summary.push(Span::raw("  •  "));
            summary.push(Span::raw(format!("{} projection(s)", total_proj)));
            out.push(Line::from(summary));

            // Per-scan facts. Only render the ones that have something
            // observable — empty scans add noise.
            for (i, s) in e.scans.iter().enumerate() {
                let prefix = if e.scans.len() > 1 {
                    format!("  scan {}: ", i + 1)
                } else {
                    "  ".to_string()
                };
                if !s.filters.is_empty() {
                    out.push(Line::from(vec![
                        Span::raw(prefix.clone()),
                        Span::styled("✓ predicate pushdown: ", ok),
                        Span::raw(s.filters.join(" AND ")),
                    ]));
                }
                if !s.projections.is_empty() {
                    let label = format!("✓ projection pushdown: {} col(s) ", s.projections.len());
                    let preview = ellipsise(&s.projections.join(", "), 60);
                    out.push(Line::from(vec![
                        Span::raw(prefix.clone()),
                        Span::styled(label, ok),
                        Span::styled(preview, dim),
                    ]));
                }
                // ANALYZE-only: show actual vs estimate per scan so the
                // user can see which scans were misestimated. We only
                // emit this row when both numbers are present and they
                // diverge enough to be interesting (>2x in either dir).
                if e.analyzed {
                    if let (Some(act), Some(est)) = (s.actual_rows, s.estimated_rows) {
                        if act != est {
                            out.push(Line::from(vec![
                                Span::raw(prefix.clone()),
                                Span::styled(
                                    format!("● actual {} rows", fmt_count(act)),
                                    analyzed_color,
                                ),
                                Span::styled(format!("  (estimated ~{})", fmt_count(est)), dim),
                            ]));
                        }
                    }
                    if let Some(n) = s.files_read {
                        if n > 1 {
                            out.push(Line::from(vec![
                                Span::raw(prefix.clone()),
                                Span::styled(format!("● scanned {} files", n), analyzed_color),
                            ]));
                        }
                    }
                    // v0.14: row-group pruning ratio from JSON profile.
                    // Color reads as a gauge: green = pruner did real
                    // work, gold = it pruned something but not much,
                    // dim = nothing pruned (often a hint that stats
                    // are missing — gen_suggestions handles that).
                    if let (Some(ratio), Some(scanned), Some(total)) =
                        (s.pruning_ratio, s.rows_scanned, s.file_total_rows)
                    {
                        let pct = (ratio * 100.0).round() as u64;
                        let style = if ratio >= 0.5 {
                            ok
                        } else if ratio > 0.0 {
                            warn
                        } else {
                            dim
                        };
                        out.push(Line::from(vec![
                            Span::raw(prefix.clone()),
                            Span::styled(
                                format!(
                                    "● pruned: {pct}% ({}/{} rows)",
                                    fmt_count(scanned),
                                    fmt_count(total)
                                ),
                                style,
                            ),
                        ]));
                    }
                }
            }

            // Heuristic suggestions — separated visually with a blank line
            // when there's anything above them.
            if !e.suggestions.is_empty() {
                if !out.is_empty() {
                    out.push(Line::from(""));
                }
                for s in &e.suggestions {
                    out.push(Line::from(Span::styled(s.clone(), warn)));
                }
            }

            // Final fallback: if every heuristic missed AND we have no
            // structured facts, render the first few raw EXPLAIN lines
            // so the panel isn't empty.
            if out.len() <= 1 {
                out.extend(
                    e.raw
                        .lines()
                        .take(6)
                        .map(|l| Line::from(Span::styled(l.to_string(), dim))),
                );
            }
            out
        }
    };

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Format a row-count for display: 0..1k as bare digits, then 1.2k, 3.4M, etc.
/// Pure cosmetic helper for the Explain panel — keeps the summary line short
/// when scans estimate billions of rows.
fn fmt_count(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    const G: u64 = 1_000_000_000;
    if n < K {
        n.to_string()
    } else if n < M {
        format!("{:.1}k", n as f64 / K as f64)
    } else if n < G {
        format!("{:.1}M", n as f64 / M as f64)
    } else {
        format!("{:.1}B", n as f64 / G as f64)
    }
}

// ─── v0.8: persisted query history ───────────────────────────────────────────

/// Resolve the on-disk history file location.
///
/// `$HOME/.pq/history` on unix, `%USERPROFILE%\.pq\history` on Windows.
/// We don't bother with XDG_DATA_HOME / Application Support nuances —
/// a single dotfile path is what users expect from history files
/// (.bash_history, .lesshst, .psql_history all work this way) and it
/// works portably with zero extra deps.
///
/// Returns None when neither $HOME nor %USERPROFILE% is set, in which
/// case we just disable persistence (history still works in-session).
fn history_path() -> Option<PathBuf> {
    let home = env::var("HOME").or_else(|_| env::var("USERPROFILE")).ok()?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".pq").join("history"))
}

/// Load history from disk. Newline-separated; blanks skipped. Newest
/// entries at the top of the file (matches in-memory ordering). All
/// errors swallowed — losing history is annoying but never blocking.
fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(HISTORY_MAX)
        .map(String::from)
        .collect()
}

/// Persist `entries` to disk, creating `$HOME/.pq/` if it doesn't yet
/// exist. Best-effort — silently no-ops on any IO error so a read-only
/// home dir / disk-full scenario never breaks the TUI.
fn save_history(entries: &[String]) {
    let Some(path) = history_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // We write the whole file each time. With HISTORY_MAX=100 and
    // typical queries < 200 chars, that's < 20 KB — irrelevant on
    // any modern filesystem. Append mode would be more efficient but
    // wouldn't handle dedup-and-promote ("retype an old query, watch
    // it pop to the top") which `record_history` does in memory.
    let body: String = entries
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let _ = fs::write(&path, body);
}

/// Render the schema-completion popup right under the textarea cursor. We
/// keep the popup small (max 8 rows, max 32 cols wide) and clamp it to stay
/// inside the screen — if the cursor is near the bottom of the query area,
/// shift the popup down by one row so it doesn't overlap the cursor.
fn render_completion(f: &mut ratatui::Frame, app: &App<'_>, query_area: Rect) {
    let Some(comp) = &app.completion else {
        return;
    };
    if comp.candidates.is_empty() {
        return;
    }

    // Compute the popup's screen anchor. tui-textarea reports cursor
    // (row, col) in 0-based char coords inside the textarea content; the
    // textarea's visible content begins at (query_area.x + 1, query_area.y + 1)
    // because of the 1-cell border.
    let (cur_row, cur_col) = app.query.cursor();
    let anchor_x = query_area
        .x
        .saturating_add(1)
        .saturating_add(cur_col as u16);
    let anchor_y = query_area
        .y
        .saturating_add(1)
        .saturating_add(cur_row as u16)
        .saturating_add(1); // one row below the cursor

    let n = comp.candidates.len().min(8) as u16;
    // Width: longest candidate + 2 cells padding, capped at 32. Min 16 so
    // the title isn't truncated.
    let max_label = comp
        .candidates
        .iter()
        .take(8)
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let popup_w = max_label.saturating_add(4).clamp(16, 32);
    let popup_h = n.saturating_add(2); // +2 for borders

    // Clamp to screen so we never paint outside the terminal.
    let screen = f.area();
    let x = anchor_x.min(screen.x + screen.width.saturating_sub(popup_w));
    let y = anchor_y.min(screen.y + screen.height.saturating_sub(popup_h));
    let popup = Rect::new(x, y, popup_w, popup_h);
    f.render_widget(Clear, popup);

    let prefix_lower = comp.prefix.to_ascii_lowercase();
    let items: Vec<ListItem> = comp
        .candidates
        .iter()
        .enumerate()
        .take(8)
        .map(|(i, name)| {
            let is_sel = i == comp.selected;
            // Highlight the matched prefix in yellow inside each row, so the
            // user sees *why* the candidate is in the list.
            let lower = name.to_ascii_lowercase();
            let mut spans: Vec<Span> = Vec::with_capacity(3);
            if let Some(pos) = lower.find(&prefix_lower) {
                let end = pos + prefix_lower.len();
                if pos > 0 {
                    spans.push(Span::raw(name[..pos].to_string()));
                }
                spans.push(Span::styled(
                    name[pos..end].to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                if end < name.len() {
                    spans.push(Span::raw(name[end..].to_string()));
                }
            } else {
                spans.push(Span::raw(name.clone()));
            }
            let line = Line::from(spans);
            let mut item = ListItem::new(line);
            if is_sel {
                item = item.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            }
            item
        })
        .collect();

    let title = if comp.candidates.len() > 8 {
        format!(
            " .{} · {} matches (showing 8) ",
            comp.prefix,
            comp.candidates.len()
        )
    } else {
        format!(" .{} · {} matches ", comp.prefix, comp.candidates.len())
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(title),
    );
    f.render_widget(list, popup);
}

fn render_help(f: &mut ratatui::Frame, full: Rect) {
    let area = centered_rect(78, 80, full);
    f.render_widget(Clear, area);

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let key = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let lines: Vec<Line> = vec![
        Line::from(Span::styled("pq tui — keys", bold)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Tab / Shift-Tab", key),
            Span::raw("    cycle focus (Columns ↔ Query ↔ Data)"),
        ]),
        Line::from(vec![
            Span::styled("  Esc / q", key),
            Span::raw("              quit (one Esc inside Query unfocuses first)"),
        ]),
        Line::from(vec![
            Span::styled("  Q", key),
            Span::raw("                    quit + print equivalent pq CLI to stdout"),
        ]),
        Line::from(vec![
            Span::styled("  Y", key),
            Span::raw("                    flash equivalent CLI in status bar"),
        ]),
        Line::from(vec![
            Span::styled("  :", key),
            Span::raw("                    toggle compiled-SQL panel"),
        ]),
        Line::from(vec![
            Span::styled("  e", key),
            Span::raw("                    toggle Explain panel (pushdown + 💡 hints)"),
        ]),
        Line::from(vec![
            Span::styled("  E", key),
            Span::raw("                    run EXPLAIN ANALYZE — actuals + per-op timing"),
        ]),
        Line::from(vec![Span::raw(
            "                       (runs in background; Esc cancels)",
        )]),
        Line::from(vec![
            Span::styled("  Ctrl-↑ / Ctrl-↓", key),
            Span::raw("       browse query history (in Query panel)"),
        ]),
        Line::from(vec![
            Span::styled("  ?", key),
            Span::raw("                    this help (any key dismisses)"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl-C", key),
            Span::raw("               force quit (works through any modal)"),
        ]),
        Line::from(""),
        Line::from(Span::styled("Semantic sync (v0.6)", bold)),
        Line::from(Span::styled(
            "  Cursor on .col / sum_col → Columns highlights ★ revenue, etc.",
            dim,
        )),
        Line::from(Span::styled(
            "  Type `.co` in Query → schema completion popup (⏎/Tab to accept)",
            dim,
        )),
        Line::from(""),
        Line::from(Span::styled("Columns panel", bold)),
        Line::from(vec![
            Span::styled("  ↑ ↓ / k j", key),
            Span::raw("            move cursor"),
        ]),
        Line::from(vec![
            Span::styled("  Space", key),
            Span::raw("                toggle column in projection"),
        ]),
        Line::from(vec![
            Span::styled("  Enter", key),
            Span::raw("                append column to projection (no toggle off)"),
        ]),
        Line::from(""),
        Line::from(Span::styled("Data panel", bold)),
        Line::from(vec![
            Span::styled("  ← → / h l", key),
            Span::raw("             move column-cursor (lights up source field)"),
        ]),
        Line::from(vec![
            Span::styled("  ↑ ↓ / j k", key),
            Span::raw("             move row-cursor"),
        ]),
        Line::from(vec![
            Span::styled("  Enter", key),
            Span::raw("                drill down — replace query with `where .col == val …`"),
        ]),
        Line::from(vec![
            Span::styled("  Backspace", key),
            Span::raw("            undo last drill-down (single level)"),
        ]),
        Line::from(""),
        Line::from(Span::styled("Query panel — DSL grammar quick ref", bold)),
        Line::from(Span::styled(
            "  stages joined by `|`; auto WHERE/HAVING routing",
            dim,
        )),
        Line::from(""),
        Line::from(Span::raw(
            "  .col, .col2                           — projection",
        )),
        Line::from(Span::raw(
            "  .col where .a > 18                    — inline where",
        )),
        Line::from(Span::raw(
            "  where .a > 18                         — filter stage",
        )),
        Line::from(Span::raw(
            "  group_by .country | count             — count per group",
        )),
        Line::from(Span::raw("  group_by .c | sum .rev | having sum_rev > 1k")),
        Line::from(Span::raw(
            "  count_distinct .user_id               — uniq aggregate",
        )),
        Line::from(Span::raw(
            "  top 10 by sum_rev / sort by .rev desc / limit 5",
        )),
        Line::from(Span::raw(
            "  distinct                              — SELECT DISTINCT",
        )),
        Line::from(Span::raw(
            "  join \"b.parquet\" on .id              — INNER (also left_/right_/full_join)",
        )),
        Line::from(Span::raw(
            "  to_csv / to_json (= to_ndjson, to_jsonl)  — line per row",
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Nested paths (v0.10) + chained UNNEST (v0.11)",
            bold,
        )),
        Line::from(Span::raw(
            "  .events[0].kind                       — list index, jq 0-idx",
        )),
        Line::from(Span::raw(
            "  .events[-1].kind                      — last element",
        )),
        Line::from(Span::raw(
            "  .events[].kind                        — UNNEST (chains in any clause)",
        )),
        Line::from(Span::raw(
            "  .metadata[\"plan\"]                    — MAP key access",
        )),
        Line::from(Span::raw(
            "  len(.tags), keys(.m), values(.m)      — collection helpers",
        )),
        Line::from(Span::raw(
            "  group_by .events[].kind | count       — UNNEST + agg",
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Full grammar: https://github.com/thehwang/parq#grammar",
            dim,
        )),
    ];

    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" ? help — press any key to dismiss "),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Compute a centered subrect that's `pct_x`% wide and `pct_y`% tall of the
/// outer area. Standard ratatui idiom — kept inline because it's small.
fn centered_rect(pct_x: u16, pct_y: u16, outer: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(outer);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

// ─── Main entry ──────────────────────────────────────────────────────────────

pub fn run(
    conn: Connection,
    file: String,
    input_fmt: InputFormat,
    preview_limit: usize,
    udfs: Vec<String>,
) -> Result<()> {
    // Make sure raw mode + alt screen are restored even if the app panics
    // mid-render. Without this, a panic unwinds past `teardown_terminal`
    // and leaves the user's terminal in raw mode (no \n→\r\n translation),
    // which makes every subsequent `pq` table output look diagonally
    // staggered until they run `stty sane`.
    install_panic_hook();

    let mut terminal = setup_terminal().context("failed to enter alternate screen")?;
    let mut app = App::new(file, &conn, input_fmt, preview_limit, udfs)?;

    let result = run_app(&mut terminal, &mut app, &conn);

    teardown_terminal(&mut terminal).context("failed to leave alternate screen")?;

    if app.print_cli_on_exit {
        // Print on the now-restored normal stream so users can copy/paste it.
        println!("{}", app.equivalent_cli());
    }
    result
}

fn run_app(terminal: &mut Tui, app: &mut App<'_>, _conn: &Connection) -> Result<()> {
    loop {
        terminal.draw(|f| render(f, app))?;

        // Throttled compile: poll with a short timeout so the typing
        // feels live. While ANALYZE or preview is in flight, poll
        // faster so the elapsed timer in the panel header ticks
        // smoothly and finished results show up within ~50 ms.
        let timeout = match app.pending_compile_at {
            Some(t) => t
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(20)),
            None if app.preview_job.is_some() || app.analyze_job.is_some() => {
                Duration::from_millis(50)
            }
            None => Duration::from_millis(200),
        };

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    on_key(app, key);
                }
            }
        }

        app.maybe_run_compile();
        // Pull any completed preview / ANALYZE result off the worker
        // channels. Cheap (single non-blocking try_recv each) so it's
        // fine to call every tick — keeps result latency under one
        // event-poll timeout without forcing a wakeup-on-completion.
        app.poll_preview();
        app.poll_analyze();

        // Auto-clear flash messages after 3s.
        if let Some((_, t)) = app.flash {
            if t.elapsed() > Duration::from_secs(3) {
                app.flash = None;
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(|e| anyhow!(e))
}

fn teardown_terminal(t: &mut Tui) -> Result<()> {
    // Best-effort: keep going even if one step errors so we never leave
    // the terminal half-restored. `disable_raw_mode` is the single most
    // important call — without it, the user's shell will mis-render every
    // subsequent multi-line command's output.
    let _ = disable_raw_mode();
    let _ = execute!(t.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
    let _ = t.show_cursor();
    Ok(())
}

/// Replace the default panic hook with one that restores the terminal
/// before printing the panic message. We do this for the whole process —
/// `pq tui` is a one-shot subcommand, so swapping the hook globally is
/// fine.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original(info);
    }));
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Convert a tui-textarea cursor (row, col) into a byte offset into the
/// joined query string (`lines().join("\n")`). The textarea reports `col`
/// in characters, so we sum char widths up to that column for the cursor's
/// row, plus full lengths (+1 for newline) for each preceding row.
///
/// Used to feed the cursor position to `Lineage::column_at`.
fn cursor_byte_offset(ta: &TextArea<'_>) -> usize {
    let (row, col) = ta.cursor();
    let lines = ta.lines();
    let mut off = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            off += line.chars().take(col).map(|c| c.len_utf8()).sum::<usize>();
            return off;
        }
        off += line.len() + 1;
    }
    // Cursor row past the buffer (shouldn't happen, but be lenient).
    off
}

/// Decide whether the cursor is currently parked just after `.<ident_chars>`,
/// and if so, what schema columns match. Returns `None` when there's nothing
/// to complete (cursor not after a `.ident` token, or zero schema matches).
///
/// Triggering rules (intentionally conservative — we'd rather miss a
/// completion opportunity than have the popup flash open spuriously):
///   - cursor must be at the *end* of an identifier run (one byte past the
///     last word char), not in the middle of one — completing inside an
///     existing word would force an awkward "where do I splice the rest?"
///     decision;
///   - the byte at `cursor - prefix_len - 1` must be `.`;
///   - the prefix must contain at least one char (no popup on bare `.`);
///   - at least one schema column must match.
fn compute_completion(ta: &TextArea<'_>, columns: &[ColumnInfo]) -> Option<Completion> {
    let buf = ta.lines().join("\n");
    let off = cursor_byte_offset(ta);
    if off == 0 || off > buf.len() {
        return None;
    }
    let bytes = buf.as_bytes();

    // Refuse to fire when we're in the *middle* of an identifier — the next
    // byte (if any) would be a word char.
    if off < bytes.len() {
        let next = bytes[off];
        if next.is_ascii_alphanumeric() || next == b'_' {
            return None;
        }
    }

    // Walk back from cursor over ident chars to find the prefix start.
    let mut prefix_start = off;
    while prefix_start > 0 {
        let b = bytes[prefix_start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            prefix_start -= 1;
        } else {
            break;
        }
    }

    // Require a `.` immediately before the prefix.
    if prefix_start == 0 || bytes[prefix_start - 1] != b'.' {
        return None;
    }
    let dot_byte = prefix_start - 1;
    let prefix = &buf[prefix_start..off];
    if prefix.is_empty() {
        return None;
    }

    // We don't want completion to fire on a *qualified* dot like `.a.col`
    // (where the user has already typed `.a` and is in the middle of `.col`).
    // Heuristic: if the byte before the leading `.` is itself an ident byte,
    // we're in a `.x.y` chain — bail.
    if dot_byte > 0 {
        let pb = bytes[dot_byte - 1];
        if pb.is_ascii_alphanumeric() || pb == b'_' {
            return None;
        }
    }

    // Score = (matches_prefix?, name) so prefix matches sort first; among
    // those, alphabetical. Substring matches are a secondary tier.
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut prefix_hits: Vec<&str> = Vec::new();
    let mut substr_hits: Vec<&str> = Vec::new();
    for c in columns {
        let cl = c.name.to_ascii_lowercase();
        if cl.starts_with(&prefix_lower) {
            prefix_hits.push(&c.name);
        } else if cl.contains(&prefix_lower) {
            substr_hits.push(&c.name);
        }
    }
    if prefix_hits.is_empty() && substr_hits.is_empty() {
        return None;
    }
    prefix_hits.sort_unstable();
    substr_hits.sort_unstable();
    let mut candidates: Vec<String> = Vec::with_capacity(prefix_hits.len() + substr_hits.len());
    candidates.extend(prefix_hits.iter().map(|s| s.to_string()));
    candidates.extend(substr_hits.iter().map(|s| s.to_string()));

    Some(Completion {
        prefix: prefix.to_string(),
        dot_byte,
        candidates,
        selected: 0,
    })
}

/// Build a drill-down query from a row in the Data panel.
///
/// Walks `headers` left-to-right; for each header that is NOT an aggregate
/// alias (per `lineage.derived`), emits a `.col == <literal>` clause using
/// the matching cell value from `row`. Joins the clauses with ` and `,
/// prefixed with `where `.
///
/// Returns None when there's nothing to filter on — happens when every
/// header is a derived alias (e.g. `count` alone), or when the row has no
/// matching cells. The caller flashes a "needs group_by" message in that
/// case.
///
/// Examples:
///   headers=["country","count"], row=["US","248"], lineage.derived=[count]
///     → Some(`where .country == "US"`)
///   headers=["country","region","sum_revenue"], row=["US","West","19065"]
///     → Some(`where .country == "US" and .region == "West"`)
///   headers=["count"], row=["1234"], lineage.derived=[count]
///     → None
fn build_drill_query(headers: &[String], row: &[String], lineage: &Lineage) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        // Skip aggregate aliases — they're not stable filter targets.
        if lineage.derived.iter().any(|d| &d.alias == h) {
            continue;
        }
        let Some(cell) = row.get(i) else { continue };
        clauses.push(drill_clause(h, cell));
    }
    if clauses.is_empty() {
        return None;
    }
    Some(format!("where {}", clauses.join(" and ")))
}

/// Format a single drill-down clause: `.<header> == <literal>` or
/// `.<header> IS NULL` for null cells. Type-detect the literal by
/// re-using the `looks_numeric` heuristic from the data-table renderer:
///   - "∅" or empty   → IS NULL
///   - true / false   → bare boolean
///   - looks_numeric  → bare number (no quotes)
///   - everything else → "double-quoted" string (matches pq DSL convention,
///     where double-quotes are rewritten to SQL single-quotes by the parser)
///
/// Strings get minimal escaping for embedded `\` and `"` — same dialect the
/// rest of the parser already accepts.
fn drill_clause(header: &str, cell: &str) -> String {
    let t = cell.trim();
    if t == "∅" || t.is_empty() {
        return format!(".{header} IS NULL");
    }
    if t == "true" || t == "false" {
        return format!(".{header} == {t}");
    }
    if looks_numeric(t) {
        return format!(".{header} == {t}");
    }
    let escaped = t.replace('\\', "\\\\").replace('"', "\\\"");
    format!(".{header} == \"{escaped}\"")
}

/// Single-quote a string for shell consumption — escape embedded single quotes
/// the POSIX way: `it's` → `'it'\''s'`.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if !s
        .chars()
        .any(|c| c.is_whitespace() || "'\"\\$`|&;<>(){}[]*?#".contains(c))
    {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("foo.parquet"), "foo.parquet");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(
            shell_quote("group_by .country | count"),
            "'group_by .country | count'"
        );
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("c = 'US'"), "'c = '\\''US'\\'''");
    }

    #[test]
    fn shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn ellipsise_fits_unchanged() {
        assert_eq!(ellipsise("alice", 5), "alice");
        assert_eq!(ellipsise("alice", 10), "alice");
    }

    #[test]
    fn ellipsise_clipped_with_marker() {
        assert_eq!(ellipsise("alice@example.com", 8), "alice@e…");
        assert_eq!(ellipsise("alice@example.com", 1), "…");
    }

    #[test]
    fn ellipsise_zero_width() {
        assert_eq!(ellipsise("anything", 0), "");
    }

    #[test]
    fn extract_filters_empty_query() {
        assert!(extract_filters("").is_empty());
        assert!(extract_filters(".email").is_empty());
    }

    #[test]
    fn extract_filters_single_where_stage() {
        let f = extract_filters("where .age > 18");
        assert_eq!(f, vec![".age > 18"]);
    }

    #[test]
    fn extract_filters_inline_where_in_projection() {
        // v0 inline shorthand: `.email where .country == "US"`
        let f = extract_filters(".email where .country == \"US\"");
        assert_eq!(f, vec![".country == \"US\""]);
    }

    #[test]
    fn extract_filters_multiple_pipe_stages() {
        let f = extract_filters("where .a > 0 | group_by .c | having count > 5");
        assert_eq!(f, vec![".a > 0", "(having) count > 5"]);
    }

    #[test]
    fn extract_filters_word_boundary_avoids_whereabouts() {
        // Bare-word "whereabouts" must not match `where` — find_word's
        // after_ok check ensures we only match on whitespace/end after.
        assert!(extract_filters(".whereabouts").is_empty());
    }

    #[test]
    fn extract_filters_bare_keyword_at_end_does_not_panic() {
        // Regression: user typing `.country, .revenue where ` (or just
        // `... where`) used to slice `stage[pos + "where ".len()..]` which
        // overran the buffer when nothing followed the keyword. The
        // tolerant slice now returns an empty expression, which we skip.
        assert!(extract_filters(".country, .revenue where").is_empty());
        assert!(extract_filters(".country, .revenue where ").is_empty());
        assert!(extract_filters("group_by .x | having").is_empty());
    }

    #[test]
    fn looks_numeric_basic() {
        assert!(looks_numeric("42"));
        assert!(looks_numeric("3.14"));
        assert!(looks_numeric("-1.5e10"));
        assert!(looks_numeric("1,234"));
        assert!(!looks_numeric("US"));
        assert!(!looks_numeric(""));
        // No digit at all → not numeric, even if all chars are punctuation.
        assert!(!looks_numeric("--"));
    }

    // ── v0.6 helpers: cursor offset + completion popup logic ────────────
    //
    // We can't easily unit-test the full TUI render path without a virtual
    // terminal, but the *logic* that decides what to highlight and what to
    // suggest is pure on a TextArea + ColumnInfo input — which is exactly
    // what these tests pin.

    fn ta_with(text: &str, cursor_byte: usize) -> TextArea<'static> {
        // Build a TextArea whose buffer is `text` and cursor lives at the
        // char that starts at `cursor_byte`. We move the cursor by inserting
        // chars one at a time and then rewinding — `tui-textarea` exposes
        // `move_cursor(CursorMove)` but no direct "set to byte offset", so
        // we approximate with insert-from-empty: insert prefix → snapshot
        // cursor → insert suffix.
        use tui_textarea::CursorMove;
        let mut ta = TextArea::default();
        ta.insert_str(text);
        // Walk back to the beginning, then forward to cursor_byte.
        ta.move_cursor(CursorMove::Top);
        ta.move_cursor(CursorMove::Head);
        let mut walked = 0usize;
        for c in text.chars() {
            if walked >= cursor_byte {
                break;
            }
            // Forward = right within line; \n traversal handled by Down+Head
            // for simplicity. (Our tests use single-line buffers.)
            assert!(c != '\n', "test helper only supports single-line input");
            ta.move_cursor(CursorMove::Forward);
            walked += c.len_utf8();
        }
        ta
    }

    fn cols(names: &[&str]) -> Vec<ColumnInfo> {
        names
            .iter()
            .map(|n| ColumnInfo {
                name: (*n).into(),
                ty: "VARCHAR".into(),
                selected: false,
            })
            .collect()
    }

    #[test]
    fn cursor_byte_offset_single_line() {
        let ta = ta_with("group_by .country", 12);
        assert_eq!(cursor_byte_offset(&ta), 12);
    }

    #[test]
    fn completion_fires_on_dot_prefix() {
        let cs = cols(&["country", "region", "revenue", "age"]);
        // Cursor at end of `.co` — should match `country`.
        let ta = ta_with(".co", 3);
        let c = compute_completion(&ta, &cs).expect("expected popup");
        assert_eq!(c.prefix, "co");
        assert_eq!(c.dot_byte, 0);
        // Prefix matches first; "country" is the only one starting with "co".
        assert_eq!(c.candidates, vec!["country".to_string()]);
        assert_eq!(c.selected, 0);
    }

    #[test]
    fn completion_prefix_then_substring() {
        // `co_owner` and `country` start with `co` (prefix tier);
        // `discount` contains `co` mid-word (substring tier);
        // `region` does not contain `co` at all (excluded).
        let cs = cols(&["country", "discount", "co_owner", "region"]);
        let ta = ta_with(".co", 3);
        let c = compute_completion(&ta, &cs).expect("expected popup");
        // Prefix tier first (alphabetical), then substring tier.
        assert_eq!(
            c.candidates,
            vec![
                "co_owner".to_string(),
                "country".to_string(),
                "discount".to_string()
            ]
        );
    }

    #[test]
    fn completion_silent_on_empty_prefix() {
        // Just typed `.` with no following letters. We *don't* show the
        // popup here on purpose (see compute_completion docstring).
        let cs = cols(&["country", "region"]);
        let ta = ta_with(".", 1);
        assert!(compute_completion(&ta, &cs).is_none());
    }

    #[test]
    fn completion_silent_inside_word() {
        // `.country` with the cursor in the middle (after `.cou`). The
        // next byte is `n` → still inside an identifier → bail.
        let cs = cols(&["country", "region"]);
        let ta = ta_with(".country", 4);
        assert!(compute_completion(&ta, &cs).is_none());
    }

    #[test]
    fn completion_silent_on_qualified_dot_chain() {
        // `.a.co` — the leading `.` of `co` is part of a `.a.co` chain.
        // We don't try to complete table-qualified column names because
        // the schema only knows the local-side column list.
        let cs = cols(&["country", "region"]);
        let ta = ta_with(".a.co", 5);
        assert!(compute_completion(&ta, &cs).is_none());
    }

    #[test]
    fn completion_silent_when_no_match() {
        let cs = cols(&["country", "region"]);
        // `.zz` matches nothing → no popup.
        let ta = ta_with(".zz", 3);
        assert!(compute_completion(&ta, &cs).is_none());
    }

    #[test]
    fn regex_escape_handles_metachars() {
        // Sanity check the homegrown escaper used for highlight pattern.
        assert_eq!(regex_escape("country"), "country");
        assert_eq!(regex_escape("a.b"), r"a\.b");
        assert_eq!(regex_escape("price$"), r"price\$");
        assert_eq!(regex_escape("col[0]"), r"col\[0\]");
    }

    // ── v0.6 drill-down: clause + query builders ────────────────────────

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn drill_clause_quotes_strings() {
        assert_eq!(drill_clause("country", "US"), r#".country == "US""#);
    }

    #[test]
    fn drill_clause_bare_numbers() {
        assert_eq!(drill_clause("age", "42"), ".age == 42");
        assert_eq!(drill_clause("price", "3.14"), ".price == 3.14");
        assert_eq!(drill_clause("delta", "-5"), ".delta == -5");
    }

    #[test]
    fn drill_clause_bare_booleans() {
        assert_eq!(drill_clause("active", "true"), ".active == true");
        assert_eq!(drill_clause("active", "false"), ".active == false");
    }

    #[test]
    fn drill_clause_null_is_null() {
        assert_eq!(drill_clause("country", "∅"), ".country IS NULL");
        assert_eq!(drill_clause("country", ""), ".country IS NULL");
    }

    #[test]
    fn drill_clause_escapes_quotes_and_backslashes() {
        // `"` inside the cell value gets backslash-escaped; the outer
        // quotes wrap the whole thing.
        assert_eq!(drill_clause("name", r#"O"Brien"#), r#".name == "O\"Brien""#);
        assert_eq!(drill_clause("path", "C:\\x"), r#".path == "C:\\x""#);
    }

    fn lineage_with_aggs(aliases: &[&str]) -> Lineage {
        // Synthesize a Lineage with given derived aliases. We don't care
        // about column_refs for drill tests.
        let derived = aliases
            .iter()
            .map(|a| crate::lineage::DerivedColumn {
                alias: (*a).into(),
                agg: "count".into(), // value irrelevant for drill build
                source: None,
            })
            .collect();
        Lineage {
            column_refs: vec![],
            derived,
        }
    }

    #[test]
    fn drill_query_single_grouping_col() {
        let headers = s(&["country", "count"]);
        let row = s(&["US", "248"]);
        let l = lineage_with_aggs(&["count"]);
        assert_eq!(
            build_drill_query(&headers, &row, &l).as_deref(),
            Some(r#"where .country == "US""#)
        );
    }

    #[test]
    fn drill_query_multi_grouping_cols() {
        let headers = s(&["country", "region", "sum_revenue"]);
        let row = s(&["US", "West", "19065"]);
        let l = lineage_with_aggs(&["sum_revenue"]);
        assert_eq!(
            build_drill_query(&headers, &row, &l).as_deref(),
            Some(r#"where .country == "US" and .region == "West""#)
        );
    }

    #[test]
    fn drill_query_returns_none_when_all_aggs() {
        // `count` alone — no grouping columns.
        let headers = s(&["count"]);
        let row = s(&["1234"]);
        let l = lineage_with_aggs(&["count"]);
        assert_eq!(build_drill_query(&headers, &row, &l), None);
    }

    #[test]
    fn drill_query_handles_null_grouping_cell() {
        let headers = s(&["country", "count"]);
        let row = s(&["∅", "17"]);
        let l = lineage_with_aggs(&["count"]);
        assert_eq!(
            build_drill_query(&headers, &row, &l).as_deref(),
            Some("where .country IS NULL")
        );
    }

    #[test]
    fn drill_query_with_numeric_grouping_value() {
        // group_by .age | count → headers ["age","count"], row ["42","100"]
        let headers = s(&["age", "count"]);
        let row = s(&["42", "100"]);
        let l = lineage_with_aggs(&["count"]);
        assert_eq!(
            build_drill_query(&headers, &row, &l).as_deref(),
            Some("where .age == 42")
        );
    }

    // ── v0.6+v0.7 explain panel: parse_explain + suggestions ──────────

    #[test]
    fn extract_hive_keys_basic() {
        assert_eq!(
            extract_hive_keys("data/dt=2026-05-01/region=us/x.parquet"),
            vec!["dt".to_string(), "region".to_string()]
        );
    }

    #[test]
    fn extract_hive_keys_dedups_and_skips_non_hive() {
        // No `=` segments → no keys.
        assert!(extract_hive_keys("plain/path/file.parquet").is_empty());
        // Repeated key (e.g. nested partitioned write) collapses to one.
        let out = extract_hive_keys("a/dt=1/b/dt=2/file");
        assert_eq!(out, vec!["dt".to_string()]);
    }

    #[test]
    fn parse_explain_extracts_filters_and_projections() {
        // Synthetic but realistic-shaped EXPLAIN tree.
        let plan = "\
┌───────────────────────────┐
│       PARQUET_SCAN        │
│  ────────────────────     │
│ Filters: country='US'     │
│ Projections: country, age │
│ Estimated Cardinality: 248│
└───────────────────────────┘";
        let (scans, total) = parse_explain(plan);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].filters, vec!["country='US'"]);
        assert_eq!(
            scans[0].projections,
            vec!["country".to_string(), "age".to_string()]
        );
        assert_eq!(scans[0].estimated_rows, Some(248));
        assert_eq!(scans[0].actual_rows, None);
        assert_eq!(total, None);
    }

    #[test]
    fn parse_explain_handles_multiple_scans_for_join() {
        let plan = "\
HASH_JOIN
  PARQUET_SCAN
    Filters: a > 10
  PARQUET_SCAN
    Projections: id, name";
        let (scans, _) = parse_explain(plan);
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].filters, vec!["a > 10"]);
        assert!(scans[0].projections.is_empty());
        assert!(scans[1].filters.is_empty());
        assert_eq!(
            scans[1].projections,
            vec!["id".to_string(), "name".to_string()]
        );
    }

    #[test]
    fn parse_explain_empty_for_no_scan_plan() {
        assert!(parse_explain("").0.is_empty());
        assert!(parse_explain("┌───┐\n│ DUMMY │\n└───┘").0.is_empty());
    }

    #[test]
    fn parse_explain_handles_function_value_dedupe() {
        // Real DuckDB shape: each parquet scan repeats the operator name
        // as a Function: value. The naive parser used to count this as a
        // second scan node — the disambiguation looks at prev_nonblank's
        // trailing `:` to skip it.
        let plan = "\
┌───────────────────────────┐
│       PARQUET_SCAN        │
│  ────────────────────     │
│         Function:         │
│        READ_PARQUET       │
│                           │
│   Projections: country    │
│  Filters: country='US'    │
│         ~1 row            │
└───────────────────────────┘";
        let (scans, _) = parse_explain(plan);
        assert_eq!(scans.len(), 1, "expected one scan, got {:?}", scans);
        assert_eq!(scans[0].filters, vec!["country='US'"]);
        assert_eq!(scans[0].projections, vec!["country".to_string()]);
        assert_eq!(scans[0].estimated_rows, Some(1));
    }

    #[test]
    fn parse_explain_handles_tilde_cardinality_with_commas() {
        let plan = "\
PARQUET_SCAN
  Projections: id
  ~12,481 rows";
        let (scans, _) = parse_explain(plan);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].estimated_rows, Some(12_481));
    }

    // ── v0.7: ANALYZE-shape parsing ───────────────────────────────────

    #[test]
    fn parse_explain_analyze_extracts_actuals_and_total_time() {
        // Trimmed-down real-world EXPLAIN ANALYZE output: TABLE_SCAN as
        // operator, bare `N rows` (no ~) for actuals, Total Time at top.
        let plan = "\
Total Time: 0.0052s
EXPLAIN_ANALYZE
TABLE_SCAN
  Function:
  READ_PARQUET
  Projections: country
  Filters: country='US'
  Total Files Read: 1
  3 rows
  0.00s";
        let (scans, total) = parse_explain(plan);
        assert_eq!(scans.len(), 1, "expected one scan, got {:?}", scans);
        assert_eq!(scans[0].filters, vec!["country='US'"]);
        assert_eq!(scans[0].projections, vec!["country".to_string()]);
        assert_eq!(scans[0].actual_rows, Some(3));
        assert_eq!(scans[0].files_read, Some(1));
        assert_eq!(total, Some(0.0052));
    }

    #[test]
    fn parse_explain_analyze_keeps_estimate_and_actual_separately() {
        // Some plans show both ~N rows (estimate) and N rows (actual).
        let plan = "\
TABLE_SCAN
  Function:
  READ_PARQUET
  ~1 row
  248 rows";
        let (scans, _) = parse_explain(plan);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].estimated_rows, Some(1));
        assert_eq!(scans[0].actual_rows, Some(248));
    }

    #[test]
    fn parse_explain_total_time_only_set_for_analyze() {
        // Plain EXPLAIN never emits Total Time:
        let (_, total) = parse_explain("PARQUET_SCAN\n  ~1 row");
        assert_eq!(total, None);
    }

    // ── v0.14: JSON profile parsing for row-group pruning ──────────────────

    fn json_scan_node(scan_rows: u64, filename: &str) -> serde_json::Value {
        // `operator_cardinality` is the post-pushdown row count (what
        // we actually want for the pruning gauge). Real DuckDB profiles
        // also carry `operator_rows_scanned` but that field is unsafe
        // to use — see the comment in walk_json_for_parquet for why.
        serde_json::json!({
            "operator_name": "READ_PARQUET",
            "operator_type": "TABLE_SCAN",
            "operator_cardinality": scan_rows,
            "operator_rows_scanned": scan_rows * 10, // realistic decoy
            "extra_info": {
                "Function": "READ_PARQUET",
                "Filename(s)": filename,
                "Total Files Read": "1",
            },
            "children": []
        })
    }

    #[test]
    fn collect_json_parquet_scans_single_node() {
        let profile = serde_json::json!({
            "latency": 0.01,
            "children": [json_scan_node(2400, "/tmp/x.parquet")]
        });
        let scans = collect_json_parquet_scans(&profile);
        assert_eq!(scans, vec![(2400, "/tmp/x.parquet".to_string())]);
    }

    #[test]
    fn collect_json_parquet_scans_handles_nested_children() {
        // Real plan tree: BATCH_CREATE_TABLE_AS → PROJECTION → TABLE_SCAN.
        // We have to walk down to find READ_PARQUET nodes.
        let profile = serde_json::json!({
            "operator_name": "BATCH_CREATE_TABLE_AS",
            "children": [{
                "operator_name": "PROJECTION",
                "children": [
                    json_scan_node(7, "/tmp/a.parquet"),
                    json_scan_node(11, "/tmp/b.parquet"),
                ]
            }]
        });
        let scans = collect_json_parquet_scans(&profile);
        assert_eq!(
            scans,
            vec![
                (7, "/tmp/a.parquet".to_string()),
                (11, "/tmp/b.parquet".to_string())
            ]
        );
    }

    #[test]
    fn collect_json_parquet_scans_skips_missing_fields() {
        // Node missing operator_cardinality → skipped. Node missing
        // Filename(s) in extra_info → skipped. We don't panic on either.
        let profile = serde_json::json!({
            "children": [
                {
                    "operator_name": "READ_PARQUET",
                    "extra_info": {"Filename(s)": "/tmp/no-rows.parquet"},
                    "children": []
                },
                {
                    "operator_name": "READ_PARQUET",
                    "operator_cardinality": 5,
                    "extra_info": {},
                    "children": []
                },
                json_scan_node(42, "/tmp/good.parquet")
            ]
        });
        let scans = collect_json_parquet_scans(&profile);
        assert_eq!(scans, vec![(42, "/tmp/good.parquet".to_string())]);
    }

    #[test]
    fn collect_json_parquet_scans_glob_filename_passthrough() {
        // Critical regression guard: DuckDB returns the LITERAL glob in
        // Filename(s) for multi-file scans (we verified this against
        // 1.5.3). We must not split on commas — the glob is one string.
        let profile = serde_json::json!({
            "children": [json_scan_node(30, "/tmp/parts/*.parquet")]
        });
        let scans = collect_json_parquet_scans(&profile);
        assert_eq!(scans, vec![(30, "/tmp/parts/*.parquet".to_string())]);
    }

    #[test]
    fn collect_json_parquet_scans_ignores_non_parquet_operators() {
        // Common non-parquet operators in plans we care about.
        let profile = serde_json::json!({
            "operator_name": "PROJECTION",
            "children": [
                {"operator_name": "FILTER", "children": []},
                {"operator_name": "EMPTY_RESULT", "children": []},
                {"operator_name": "BATCH_CREATE_TABLE_AS", "children": []},
            ]
        });
        let scans = collect_json_parquet_scans(&profile);
        assert!(
            scans.is_empty(),
            "non-parquet ops must not be collected: {scans:?}"
        );
    }

    /// v0.14.1 regression for #12. The naive reset of
    /// `PRAGMA enable_profiling='json'` is `PRAGMA disable_profiling`,
    /// but against DuckDB 1.10.501 that's a silent no-op — the profile
    /// state stays in JSON mode and subsequent EXPLAIN ANALYZE calls
    /// return JSON in column 1, which when rendered as the Explain
    /// panel's `e.raw` fallback or as the Data panel's row content
    /// looks like garbage on screen.
    ///
    /// The test opens a fresh in-memory duckdb connection (so we don't
    /// depend on pq's open_conn or any test fixtures), runs the full
    /// `try_collect_json_scans` round-trip, and then asserts that a
    /// follow-up plain EXPLAIN returns text-shaped output, NOT JSON.
    #[test]
    fn try_collect_json_scans_resets_profile_for_subsequent_explains() {
        let conn = duckdb::Connection::open_in_memory().expect("in-memory conn");
        // Make a tiny parquet so EXPLAIN ANALYZE has something real to
        // run against. Could mock with VALUES, but a parquet path
        // exercises the same code path the TUI does.
        let path = std::env::temp_dir().join(format!(
            "pq-pragma-reset-{}-{}.parquet",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_str = path.to_string_lossy().to_string();
        conn.execute(
            &format!("COPY (SELECT range AS id FROM range(100)) TO '{path_str}'"),
            [],
        )
        .expect("copy");

        let sql = format!("SELECT id FROM read_parquet('{path_str}') WHERE id < 5");

        // Round-trip: this must leave profiling DISABLED on exit.
        let _ = try_collect_json_scans(&conn, &sql);

        // Now check that a subsequent plain EXPLAIN returns text, not
        // JSON. The text plan starts with a box-drawing char (┌); a
        // JSON document starts with `{`. We sniff the first non-whitespace
        // char rather than trying to parse, so the test stays readable.
        let mut stmt = conn.prepare(&format!("EXPLAIN {sql}")).unwrap();
        let mut rows = stmt.query([]).unwrap();
        let row = rows.next().unwrap().expect("plan row");
        let plan: String = row.get(1).unwrap();
        let first = plan.trim_start().chars().next().unwrap_or(' ');

        let _ = std::fs::remove_file(&path);

        assert_ne!(
            first,
            '{',
            "PRAGMA reset failed — EXPLAIN returned JSON ({first:?} prefix). \
             Plan starts with: {prefix}",
            prefix = &plan[..plan.len().min(80)]
        );
    }

    #[test]
    fn pruning_ratio_clamps_to_zero_one_range() {
        // Normal case: 80% pruned.
        assert!((pruning_ratio(2_000, 10_000) - 0.8).abs() < 1e-9);
        // operator_rows_scanned > total (parallelism / double-counting
        // observed in older DuckDB) clamps to 0 instead of going negative.
        assert_eq!(pruning_ratio(15_000, 10_000), 0.0);
        // Zero total (couldn't read metadata) is treated as no info.
        assert_eq!(pruning_ratio(100, 0), 0.0);
        // Full prune.
        assert_eq!(pruning_ratio(0, 10_000), 1.0);
    }

    #[test]
    fn gen_suggestions_hive_partition_unfiltered() {
        let out = gen_suggestions(
            &[ScanInfo::default()],
            "data/dt=2026-05-01/region=us/x.parquet",
            ".email",
            false,
        );
        // Both partition keys flagged.
        assert!(out.iter().any(|s| s.contains("`where .dt =")));
        assert!(out.iter().any(|s| s.contains("`where .region =")));
    }

    #[test]
    fn gen_suggestions_hive_partition_satisfied_no_hint() {
        let out = gen_suggestions(
            &[ScanInfo::default()],
            "data/dt=2026-05-01/x.parquet",
            "where .dt == \"2026-05-01\"",
            false,
        );
        // Query already references .dt → no partition hint for it.
        assert!(out.iter().all(|s| !s.contains("`where .dt =")));
    }

    #[test]
    fn gen_suggestions_filter_did_not_push() {
        // User wrote a where, but EXPLAIN shows no pushdown — surface that.
        let out = gen_suggestions(&[ScanInfo::default()], "x.parquet", "where .a > 18", false);
        assert!(
            out.iter()
                .any(|s| s.contains("didn't push to the parquet scan")),
            "expected pushdown warning, got {:?}",
            out
        );
    }

    #[test]
    fn gen_suggestions_no_projection_pushdown_on_bare_query() {
        // Empty query (default LIMIT preview) reads all columns — flag it.
        let out = gen_suggestions(&[ScanInfo::default()], "x.parquet", "", false);
        assert!(
            out.iter().any(|s| s.contains("select specific columns")),
            "expected projection hint, got {:?}",
            out
        );
    }

    #[test]
    fn gen_suggestions_stale_stats_only_when_analyzed() {
        // 100x divergence (estimate=10, actual=1000) — fires only when
        // analyzed=true (we trust the actuals only after running ANALYZE).
        let scan = ScanInfo {
            estimated_rows: Some(10),
            actual_rows: Some(1000),
            ..Default::default()
        };
        let out_plain = gen_suggestions(std::slice::from_ref(&scan), "x.parquet", ".x", false);
        assert!(
            out_plain.iter().all(|s| !s.contains("estimate skewed")),
            "plain EXPLAIN must not surface stale-stats hint"
        );
        let out_analyzed = gen_suggestions(&[scan], "x.parquet", ".x", true);
        assert!(
            out_analyzed.iter().any(|s| s.contains("estimate skewed")),
            "ANALYZE should fire stale-stats hint, got {:?}",
            out_analyzed
        );
    }

    #[test]
    fn gen_suggestions_no_stale_stats_when_within_10x() {
        // 5x divergence — under threshold, no hint.
        let scan = ScanInfo {
            estimated_rows: Some(100),
            actual_rows: Some(500),
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "x.parquet", ".x", true);
        assert!(out.iter().all(|s| !s.contains("estimate skewed")));
    }

    #[test]
    fn gen_suggestions_many_files_no_predicate() {
        let scan = ScanInfo {
            files_read: Some(50),
            // No filters → no pushed predicate.
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "data/x.parquet", ".col", true);
        assert!(
            out.iter()
                .any(|s| s.contains("scanned 50 files with no pushed predicate")),
            "expected many-files hint, got {:?}",
            out
        );
    }

    // ── v0.14: row-group-pruning hint ──────────────────────────────────────

    #[test]
    fn gen_suggestions_zero_prune_with_filter_emits_hint() {
        // Big file (10M rows) + filter present + pruning_ratio=0
        // → fire the "stats may be missing" hint.
        let scan = ScanInfo {
            filters: vec!["country = 'US'".into()],
            pruning_ratio: Some(0.0),
            rows_scanned: Some(10_000_000),
            file_total_rows: Some(10_000_000),
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "x.parquet", "where .country == \"US\"", true);
        assert!(
            out.iter()
                .any(|s| s.contains("didn't prune any row groups")),
            "expected zero-prune hint, got {out:?}"
        );
        assert!(
            out.iter().any(|s| s.contains("country = 'US'")),
            "hint should name the filter, got {out:?}"
        );
    }

    #[test]
    fn gen_suggestions_zero_prune_without_filter_no_hint() {
        // No filter pushed → can't blame row-group pruning, no hint.
        let scan = ScanInfo {
            filters: vec![],
            pruning_ratio: Some(0.0),
            rows_scanned: Some(10_000_000),
            file_total_rows: Some(10_000_000),
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "x.parquet", "", true);
        assert!(
            out.iter()
                .all(|s| !s.contains("didn't prune any row groups")),
            "no-filter case must not emit zero-prune hint, got {out:?}"
        );
    }

    #[test]
    fn gen_suggestions_zero_prune_below_size_threshold_no_hint() {
        // Tiny file (50k rows) — single row group anyway, pruning isn't
        // physically possible. Suppress the hint to keep the panel
        // signal-to-noise high.
        let scan = ScanInfo {
            filters: vec!["country = 'US'".into()],
            pruning_ratio: Some(0.0),
            rows_scanned: Some(50_000),
            file_total_rows: Some(50_000),
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "small.parquet", "where .country == \"US\"", true);
        assert!(
            out.iter()
                .all(|s| !s.contains("didn't prune any row groups")),
            "small-file case must not emit zero-prune hint, got {out:?}"
        );
    }

    #[test]
    fn gen_suggestions_partial_prune_no_zero_hint() {
        // Pruner did SOME work (50%) — the hint is specifically for
        // the "stats are missing" case where pruning is exactly zero.
        let scan = ScanInfo {
            filters: vec!["country = 'US'".into()],
            pruning_ratio: Some(0.5),
            rows_scanned: Some(5_000_000),
            file_total_rows: Some(10_000_000),
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "x.parquet", "where .country == \"US\"", true);
        assert!(
            out.iter()
                .all(|s| !s.contains("didn't prune any row groups")),
            "partial-prune must not emit zero-prune hint, got {out:?}"
        );
    }

    #[test]
    fn gen_suggestions_zero_prune_only_when_analyzed() {
        // Plain EXPLAIN can't have pruning_ratio (it's analyze-only); even
        // if the field were somehow set, we don't emit the hint without
        // analyze=true since the actual numbers aren't trustworthy.
        let scan = ScanInfo {
            filters: vec!["country = 'US'".into()],
            pruning_ratio: Some(0.0),
            rows_scanned: Some(10_000_000),
            file_total_rows: Some(10_000_000),
            ..Default::default()
        };
        let out = gen_suggestions(&[scan], "x.parquet", "where .country == \"US\"", false);
        assert!(
            out.iter()
                .all(|s| !s.contains("didn't prune any row groups")),
            "plain EXPLAIN must not surface pruning hint, got {out:?}"
        );
    }

    #[test]
    fn fmt_count_formats_humanly() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_500), "1.5k");
        assert_eq!(fmt_count(2_400_000), "2.4M");
        assert_eq!(fmt_count(7_800_000_000), "7.8B");
    }

    // ── v0.8: history bookkeeping ───────────────────────────────────────────

    #[test]
    fn history_dedupes_consecutive_writes() {
        let mut app = App::for_test("demo.parquet", Vec::new(), Preview::default());
        app.record_history(".country".into());
        app.record_history(".country".into());
        assert_eq!(app.history, vec![".country".to_string()]);
    }

    #[test]
    fn history_promotes_repeat_to_top() {
        let mut app = App::for_test("demo.parquet", Vec::new(), Preview::default());
        app.record_history(".a".into());
        app.record_history(".b".into());
        app.record_history(".a".into());
        assert_eq!(
            app.history,
            vec![".a".to_string(), ".b".to_string()],
            "retyped query should promote, not duplicate"
        );
    }

    #[test]
    fn history_caps_at_history_max() {
        let mut app = App::for_test("demo.parquet", Vec::new(), Preview::default());
        for i in 0..(HISTORY_MAX + 10) {
            app.record_history(format!(".col{i}"));
        }
        assert_eq!(app.history.len(), HISTORY_MAX);
        // Newest at the head, oldest dropped.
        assert!(app
            .history
            .first()
            .unwrap()
            .ends_with(&format!("{}", HISTORY_MAX + 9)));
    }

    // ── v0.11: CLI round-trip when the TUI was launched with `-i` / `-n` /
    //   `--udf`. Y / Q exits print this string; users paste it back into a
    //   shell, and it must reconstruct the exact same session. Previously
    //   we dropped the format and limit, so a TUI that was reading an
    //   `.ndjson` would print `pq f.ndjson '...'` — which `pq` would then
    //   sniff back as parquet from the extension... usually. The test
    //   pins explicit -i emission so that fragile heuristic doesn't get
    //   used as the contract.

    #[test]
    fn equivalent_cli_omits_defaults() {
        let app = App::for_test("demo.parquet", Vec::new(), Preview::default());
        assert_eq!(app.equivalent_cli(), "pq demo.parquet");
    }

    #[test]
    fn equivalent_cli_includes_input_format_for_ndjson() {
        let mut app = App::for_test("demo.ndjson", Vec::new(), Preview::default());
        app.input_fmt = InputFormat::Ndjson;
        // No query yet — still emit `-i ndjson` so paste-back doesn't fall
        // through to extension sniffing (which happens to agree here, but
        // the format flag is the source of truth in the TUI session).
        assert_eq!(app.equivalent_cli(), "pq -i ndjson demo.ndjson");
    }

    #[test]
    fn equivalent_cli_includes_csv_and_n() {
        let mut app = App::for_test("data.csv", Vec::new(), Preview::default());
        app.input_fmt = InputFormat::Csv;
        app.preview_limit = 200;
        app.query.insert_str("where .age > 18");
        let cli = app.equivalent_cli();
        assert!(
            cli.starts_with("pq -i csv -n 200 "),
            "expected csv+n prefix, got {cli}"
        );
        assert!(cli.ends_with("'where .age > 18'"), "got {cli}");
    }

    #[test]
    fn equivalent_cli_threads_udfs() {
        let mut app = App::for_test("demo.parquet", Vec::new(), Preview::default());
        app.udfs = vec![
            "is_us(c) := c = 'US'".to_string(),
            "shout(s) := upper(s)".to_string(),
        ];
        let cli = app.equivalent_cli();
        // Both --udf flags must show up, single-quoted because the body
        // contains spaces and `=`. We don't pin exact ordering because
        // shell_quote escapes interact with the user's macro contents
        // — match on substring so the test stays useful when shell_quote
        // changes its escape strategy.
        assert!(
            cli.contains("--udf 'is_us(c) := c = '\\''US'\\'''"),
            "missing first udf in {cli}"
        );
        assert!(
            cli.contains("--udf 'shout(s) := upper(s)'"),
            "missing second udf in {cli}"
        );
    }

    // ── v0.11: schema fetch + preview compile route through the format,
    //   so non-parquet TUI sessions DTRT. We test against an in-memory
    //   DuckDB connection because the bug we fixed (always reading
    //   parquet) only manifests at SQL-compile time.

    #[test]
    fn fetch_schema_uses_format_for_ndjson() {
        // Write a tiny ndjson fixture, ask the TUI's schema fetcher to
        // describe it. Before v0.11 this synthesized read_parquet() and
        // duckdb's binder rejected the call; now it picks read_json.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tiny.ndjson");
        std::fs::write(&path, "{\"a\":1,\"b\":\"x\"}\n{\"a\":2,\"b\":\"y\"}\n").unwrap();
        let conn = Connection::open_in_memory().expect("open conn");
        let cols =
            fetch_schema(&conn, path.to_str().unwrap(), InputFormat::Ndjson).expect("schema");
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn run_preview_uses_format_for_ndjson() {
        // End-to-end: ndjson source + a typical DSL stage (where + count).
        // Validates that compile_plan_fmt is on the hot path and the
        // generated SQL hits read_json, not read_parquet.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tiny.ndjson");
        std::fs::write(
            &path,
            "{\"country\":\"US\"}\n{\"country\":\"DE\"}\n{\"country\":\"US\"}\n",
        )
        .unwrap();
        let conn = Connection::open_in_memory().expect("open conn");
        let preview = run_preview(
            &conn,
            path.to_str().unwrap(),
            "where .country == \"US\" | count",
            InputFormat::Ndjson,
            50,
        );
        assert!(
            preview.error.is_none(),
            "preview errored: {:?}",
            preview.error
        );
        assert!(
            preview.sql.contains("read_json("),
            "expected read_json in compiled sql, got: {}",
            preview.sql
        );
        assert_eq!(preview.headers, vec!["count".to_string()]);
        assert_eq!(preview.rows, vec![vec!["2".to_string()]]);
    }
}

// ── v0.8: TUI rendering snapshot tests ───────────────────────────────────────
//
// Drive `render` against a fake App with hand-built Preview/columns and
// snapshot the resulting text buffer with insta. Catches regressions in
// layout, panel labels, status-bar wording, completion popup geometry,
// help overlay, etc. — anywhere structural breakage would otherwise
// require a human to spot in a screenshot.
//
// Snapshots strip ANSI styling; they only cover *characters*. Color and
// emphasis regressions are out of scope (would force snapshot churn on
// every palette tweak). When a test fails, regenerate with:
//
//   cargo insta review     # interactive
//   cargo insta accept     # accept all pending
//
// New snapshots are committed under src/snapshots/.
#[cfg(test)]
mod snapshots {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    fn make_columns() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo {
                name: "id".into(),
                ty: "BIGINT".into(),
                selected: false,
            },
            ColumnInfo {
                name: "country".into(),
                ty: "VARCHAR".into(),
                selected: false,
            },
            ColumnInfo {
                name: "revenue".into(),
                ty: "DOUBLE".into(),
                selected: false,
            },
        ]
    }

    fn make_preview() -> Preview {
        Preview {
            headers: vec!["country".into(), "revenue".into()],
            rows: vec![
                vec!["US".into(), "100.50".into()],
                vec!["DE".into(), "85.00".into()],
                vec!["JP".into(), "63.20".into()],
            ],
            sql: "SELECT country, revenue FROM read_parquet('demo.parquet') LIMIT 50".into(),
            last_ms: 12,
            error: None,
            explain: None,
        }
    }

    fn buffer_to_text(buf: &Buffer) -> String {
        let area = buf.area();
        (0..area.height)
            .map(|y| {
                let line: String = (0..area.width)
                    .map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol())
                            .unwrap_or_default()
                    })
                    .collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_to_string(app: &mut App<'_>, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal.draw(|f| render(f, app)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        buffer_to_text(&buf)
    }

    /// Insta wrapper that uses a stable, descriptive name and disables
    /// the file/line prefix so renames don't churn the snapshot tree.
    macro_rules! snap {
        ($name:literal, $body:expr) => {{
            insta::with_settings!({ omit_expression => true }, {
                insta::assert_snapshot!($name, $body);
            });
        }};
    }

    #[test]
    fn snap_empty() {
        let mut app = App::for_test("demo.parquet", make_columns(), Preview::default());
        let s = render_to_string(&mut app, 100, 24);
        snap!("empty", s);
    }

    #[test]
    fn snap_with_results() {
        let mut app = App::for_test("demo.parquet", make_columns(), make_preview());
        // Mark country/revenue as selected (string-match would do this in
        // real life when the user types `.country, .revenue`).
        app.columns[1].selected = true;
        app.columns[2].selected = true;
        app.query.insert_str(".country, .revenue");
        let s = render_to_string(&mut app, 100, 24);
        snap!("with_results", s);
    }

    #[test]
    fn snap_show_sql() {
        let mut app = App::for_test("demo.parquet", make_columns(), make_preview());
        app.show_sql = true;
        let s = render_to_string(&mut app, 100, 24);
        snap!("show_sql", s);
    }

    #[test]
    fn snap_explain_panel_estimates() {
        let mut preview = make_preview();
        preview.explain = Some(ExplainSummary {
            scans: vec![ScanInfo {
                filters: vec!["country='US'".into()],
                projections: vec!["country".into(), "revenue".into()],
                estimated_rows: Some(1234),
                actual_rows: None,
                files_read: None,
                rows_scanned: None,
                file_total_rows: None,
                pruning_ratio: None,
            }],
            suggestions: vec!["💡 add `where .partition_date >= ...` to prune row groups".into()],
            raw: "PARQUET_SCAN ~1234 rows".into(),
            analyzed: false,
            total_seconds: None,
        });
        let mut app = App::for_test("demo.parquet", make_columns(), preview);
        app.show_explain = true;
        let s = render_to_string(&mut app, 110, 28);
        snap!("explain_panel_estimates", s);
    }

    #[test]
    fn snap_analyze_completed() {
        let mut preview = make_preview();
        preview.explain = Some(ExplainSummary {
            scans: vec![ScanInfo {
                filters: vec!["country='US'".into()],
                projections: vec!["country".into(), "revenue".into()],
                estimated_rows: Some(1234),
                actual_rows: Some(1100),
                files_read: Some(3),
                rows_scanned: None,
                file_total_rows: None,
                pruning_ratio: None,
            }],
            suggestions: vec![],
            raw: "PARQUET_SCAN ~1234 rows / 1100 rows".into(),
            analyzed: true,
            total_seconds: Some(0.0152),
        });
        let mut app = App::for_test("demo.parquet", make_columns(), preview);
        app.show_explain = true;
        let s = render_to_string(&mut app, 110, 28);
        snap!("analyze_completed", s);
    }

    #[test]
    fn snap_explain_pruning_active() {
        // v0.14: ANALYZE summary with pruning ratio populated. The new
        // "● pruned: 80% (...)" row should appear in the per-scan
        // section and the rest of the panel renders unchanged.
        let mut preview = make_preview();
        preview.explain = Some(ExplainSummary {
            scans: vec![ScanInfo {
                filters: vec!["country='US'".into()],
                projections: vec!["country".into(), "revenue".into()],
                estimated_rows: Some(12_000),
                actual_rows: Some(2_400),
                files_read: Some(1),
                rows_scanned: Some(2_400),
                file_total_rows: Some(12_000),
                pruning_ratio: Some(0.8),
            }],
            suggestions: vec![],
            raw: "PARQUET_SCAN ~12000 rows / 2400 rows".into(),
            analyzed: true,
            total_seconds: Some(0.022),
        });
        let mut app = App::for_test("demo.parquet", make_columns(), preview);
        app.show_explain = true;
        let s = render_to_string(&mut app, 110, 28);
        snap!("explain_pruning_active", s);
    }

    #[test]
    fn snap_completion_popup() {
        let mut app = App::for_test("demo.parquet", make_columns(), make_preview());
        app.focus = Panel::Query;
        app.query.insert_str(".co");
        // Drive the real completion logic so the popup state matches what
        // a user would see — keeps the snapshot honest if the matching
        // logic changes (e.g. adds fuzzy ranking).
        app.refresh_completion();
        let s = render_to_string(&mut app, 100, 24);
        snap!("completion_popup", s);
    }

    #[test]
    fn snap_help_overlay() {
        let mut app = App::for_test("demo.parquet", make_columns(), make_preview());
        app.show_help = true;
        let s = render_to_string(&mut app, 100, 30);
        snap!("help_overlay", s);
    }

    #[test]
    fn snap_error_state() {
        let preview = Preview {
            error: Some("parse: unknown column .nope".into()),
            ..Default::default()
        };
        let mut app = App::for_test("demo.parquet", make_columns(), preview);
        app.query.insert_str(".nope");
        let s = render_to_string(&mut app, 100, 24);
        snap!("error_state", s);
    }

    #[test]
    fn snap_drill_down_active() {
        let mut app = App::for_test("demo.parquet", make_columns(), make_preview());
        app.focus = Panel::Data;
        app.data_row_idx = Some(0);
        app.data_col_idx = Some(0);
        // Pretend we drilled down — the tell is `drill_undo` being set,
        // which the status bar surfaces as "↶ drill-undo: backspace".
        app.drill_undo = Some(".country, .revenue".into());
        app.query.insert_str("where .country == \"US\"");
        let s = render_to_string(&mut app, 100, 24);
        snap!("drill_down_active", s);
    }
}
