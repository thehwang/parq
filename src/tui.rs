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
                    self.last_compiled = q;
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
    if app.query.input(key) {
        app.schedule_compile();
    }
}

fn on_key_data(app: &mut App<'_>, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('Q') => {
            app.should_quit = true;
            app.print_cli_on_exit = true;
        }
        KeyCode::Char('Y') => app.copy_cli_to_clipboard(),
        KeyCode::Char(':') => app.show_sql = !app.show_sql,
        KeyCode::Char('?') => app.show_help = true,
        // Arrow keys: scroll the data table in v0.6 (currently shows top 50).
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

    render_query(f, app, right[0]);
    if app.show_sql {
        render_sql(f, app, right[1]);
        render_data(f, app, right[2]);
    } else {
        render_data(f, app, right[1]);
    }

    render_status_bar(f, app, outer[1]);

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
    let items: Vec<ListItem> = app
        .columns
        .iter()
        .map(|c| {
            let mark = if c.selected { "✓ " } else { "  " };
            let line = Line::from(vec![
                Span::raw(mark),
                Span::styled(&c.name, Style::default().add_modifier(Modifier::BOLD)),
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

    let header = Row::new(app.preview.headers.iter().enumerate().map(|(i, h)| {
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if numeric[i] {
            style = style.fg(Color::Cyan);
        }
        let cell = Cell::from(h.clone()).style(style);
        if numeric[i] {
            cell.style(style)
        } else {
            cell
        }
    }));

    let rows: Vec<Row> = app
        .preview
        .rows
        .iter()
        .map(|r| {
            Row::new(r.iter().enumerate().map(|(i, c)| {
                let w = col_widths[i] as usize;
                let truncated = ellipsise(c, w);
                let text = if numeric.get(i).copied().unwrap_or(false) {
                    // Right-align numerics by padding on the left to col width.
                    format!("{:>w$}", truncated, w = w)
                } else {
                    truncated
                };
                Cell::from(text)
            }))
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
    let default_help =
        " Tab next │ ␣ toggle col │ ⏎ append │ Q exit+print │ Esc/q quit │ : SQL │ ? help ";
    let text = match &app.flash {
        Some((msg, _)) => msg.clone(),
        None => default_help.to_string(),
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
fn extract_filters(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    for stage in q.split('|').map(str::trim) {
        if stage.is_empty() {
            continue;
        }
        for kw in &["where ", "having "] {
            if let Some(pos) = find_word(stage, kw.trim_end()) {
                let expr = stage[pos + kw.len()..].trim().to_string();
                if !expr.is_empty() {
                    let prefix = if *kw == "having " { "(having) " } else { "" };
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
}
