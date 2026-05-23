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

use std::io::{self, Stdout};
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
use crate::parser;

type Tui = Terminal<CrosstermBackend<Stdout>>;

const PREVIEW_LIMIT: usize = 50;
const SQL_THROTTLE: Duration = Duration::from_millis(50);

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

fn fetch_schema(conn: &Connection, file: &str) -> Result<Vec<ColumnInfo>> {
    let src = parser::source_clause(file);
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
    /// Up to PREVIEW_LIMIT rows; each cell is preformatted text.
    rows: Vec<Vec<String>>,
    /// Compiled SQL (for the `:` SQL viewer).
    sql: String,
    /// Wall-clock spent in execute() — shown in the Query panel header.
    last_ms: u128,
    /// Set when the most recent compile/execute failed; cleared on next success.
    error: Option<String>,
}

fn run_preview(conn: &Connection, file: &str, query: &str) -> Preview {
    let started = Instant::now();
    let plan = match parser::compile_plan(file, query, PREVIEW_LIMIT) {
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
        plan.sql, PREVIEW_LIMIT
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

    let mut rows = Vec::with_capacity(PREVIEW_LIMIT);
    while let Ok(Some(row)) = rows_iter.next() {
        let mut cells = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: Value = row.get(i).unwrap_or(Value::Null);
            cells.push(value_to_string(&v));
        }
        rows.push(cells);
    }
    Preview {
        headers,
        rows,
        sql: plan.sql,
        last_ms: started.elapsed().as_millis(),
        error: None,
    }
}

/// Stripped-down version of output::value_to_display — duplicated here to keep
/// the TUI module self-contained (output.rs has terminal-rendering deps that
/// don't make sense inside ratatui).
fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => "∅".into(),
        Value::Text(s) => s.clone(),
        Value::Boolean(b) => b.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Double(f) => format!("{f}"),
        Value::Decimal(d) => d.to_string(),
        Value::TinyInt(i) => i.to_string(),
        Value::SmallInt(i) => i.to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::HugeInt(i) => i.to_string(),
        Value::UTinyInt(i) => i.to_string(),
        Value::USmallInt(i) => i.to_string(),
        Value::UInt(i) => i.to_string(),
        Value::UBigInt(i) => i.to_string(),
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
        other => format!("{other:?}"),
    }
}

// ─── App state ───────────────────────────────────────────────────────────────

struct App<'ta> {
    file: String,
    columns: Vec<ColumnInfo>,
    column_state: ListState,
    query: TextArea<'ta>,
    preview: Preview,
    focus: Panel,
    /// True when `:` is pressed → expands the compiled-SQL panel.
    show_sql: bool,
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
    fn new(file: String, conn: &Connection) -> Result<Self> {
        let columns = fetch_schema(conn, &file)?;
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
        // Default: show first 20 rows (same as bare `pq file.parquet`).
        // We leave the textarea empty, which compile_plan() expands into
        // `SELECT * FROM ... LIMIT 20`.

        let preview = run_preview(conn, &file, "");
        Ok(Self {
            file,
            columns,
            column_state,
            query,
            preview,
            focus: Panel::Columns,
            show_sql: false,
            show_help: false,
            last_compiled: String::new(),
            pending_compile_at: None,
            flash: None,
            should_quit: false,
            print_cli_on_exit: false,
            lineage: Lineage::default(),
            data_col_idx: None,
            data_row_idx: None,
            drill_undo: None,
            completion: None,
        })
    }

    fn current_query_text(&self) -> String {
        self.query.lines().join("\n")
    }

    fn equivalent_cli(&self) -> String {
        let q = self.current_query_text();
        if q.trim().is_empty() {
            format!("pq {}", shell_quote(&self.file))
        } else {
            format!("pq {} {}", shell_quote(&self.file), shell_quote(&q))
        }
    }

    fn schedule_compile(&mut self) {
        self.pending_compile_at = Some(Instant::now() + SQL_THROTTLE);
    }

    fn maybe_run_compile(&mut self, conn: &Connection) {
        if let Some(deadline) = self.pending_compile_at {
            if Instant::now() >= deadline {
                let q = self.current_query_text();
                if q != self.last_compiled {
                    self.preview = run_preview(conn, &self.file, &q);
                    self.last_compiled = q.clone();
                    // After the data refreshes, both the column- and row-
                    // cursor may now point past the end of the new
                    // headers/rows (e.g. user changed projection from 3
                    // cols to 1, or filter wiped the result set). Clamp
                    // both, dropping to None when the panel is empty so
                    // we don't render a phantom highlight.
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
                }
                self.pending_compile_at = None;
                // Re-derive column.selected from query text (string-match).
                let q_lower = self.last_compiled.to_ascii_lowercase();
                for c in &mut self.columns {
                    let needle = format!(".{}", c.name.to_ascii_lowercase());
                    c.selected = q_lower.contains(&needle);
                }
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
    // Ctrl-C always quits, regardless of any modal state.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
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
            _ => {}
        }
    }
    // Forward to the textarea.
    let consumed = app.query.input(key);
    if consumed {
        app.schedule_compile();
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

    let right_constraints = if app.show_sql {
        vec![
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Min(5),
        ]
    } else {
        vec![Constraint::Length(6), Constraint::Min(5)]
    };
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(right_constraints)
        .split(body[1]);

    let query_area = right[0];
    render_query(f, app, query_area);
    if app.show_sql {
        render_sql(f, app, right[1]);
        render_data(f, app, right[2]);
    } else {
        render_data(f, app, right[1]);
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

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focused_style(active))
                .title(format!(" Columns · {} ", app.columns.len())),
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
    let title = if let Some(err) = &app.preview.error {
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focused_style(active))
        .title(format!(
            " Data · {} of {} rows shown ",
            app.preview.rows.len(),
            app.preview.rows.len() // honest about the cap; v0.6 counts true rows
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
    let default_help = " Tab next │ ␣ toggle col │ ⏎ append/drill │ ⌫ undo drill │ Q exit+print │ Esc quit │ : SQL │ ? help ";
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
            "  where .a > 18                         — filter",
        )),
        Line::from(Span::raw(
            "  group_by .country | count             — count per group",
        )),
        Line::from(Span::raw(
            "  group_by .c | sum .rev                — sum per group",
        )),
        Line::from(Span::raw(
            "  top 10 by sum_rev                     — order desc + limit",
        )),
        Line::from(Span::raw(
            "  sort by .rev desc | limit 5           — explicit",
        )),
        Line::from(Span::raw(
            "  distinct                              — SELECT DISTINCT",
        )),
        Line::from(Span::raw(
            "  join \"b.parquet\" on .id              — INNER join",
        )),
        Line::from(Span::raw(
            "  left_join \"b\" on .a.id == .b.uid     — LEFT/RIGHT/FULL",
        )),
        Line::from(Span::raw(
            "  to_csv  /  to_json                    — line per row",
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

pub fn run(conn: Connection, file: String) -> Result<()> {
    let mut terminal = setup_terminal().context("failed to enter alternate screen")?;
    let mut app = App::new(file, &conn)?;

    let result = run_app(&mut terminal, &mut app, &conn);

    teardown_terminal(&mut terminal).context("failed to leave alternate screen")?;

    if app.print_cli_on_exit {
        // Print on the now-restored normal stream so users can copy/paste it.
        println!("{}", app.equivalent_cli());
    }
    result
}

fn run_app(terminal: &mut Tui, app: &mut App<'_>, conn: &Connection) -> Result<()> {
    loop {
        terminal.draw(|f| render(f, app))?;

        // Throttled compile: poll with a short timeout so the typing feels live.
        let timeout = match app.pending_compile_at {
            Some(t) => t
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(20)),
            None => Duration::from_millis(200),
        };

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    on_key(app, key);
                }
            }
        }

        app.maybe_run_compile(conn);

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
    disable_raw_mode()?;
    execute!(t.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    t.show_cursor()?;
    Ok(())
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
}
