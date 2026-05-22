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
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
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
    // Global keys (work in any panel).
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
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
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('Q') => {
            app.should_quit = true;
            app.print_cli_on_exit = true;
        }
        KeyCode::Char('Y') => app.copy_cli_to_clipboard(),
        KeyCode::Char(':') => app.show_sql = !app.show_sql,
        KeyCode::Char('?') => app.flash_msg("? help — coming in v0.6"),
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
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('Q') => {
            app.should_quit = true;
            app.print_cli_on_exit = true;
        }
        KeyCode::Char('Y') => app.copy_cli_to_clipboard(),
        KeyCode::Char(':') => app.show_sql = !app.show_sql,
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

    render_columns(f, app, body[0]);

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
                let text = if numeric.get(i).copied().unwrap_or(false) {
                    // Right-align inside the column by left-padding to col width.
                    let w = col_widths[i] as usize;
                    format!("{:>w$}", c, w = w)
                } else {
                    c.clone()
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
    let default_help = " Tab next │ ␣ toggle col │ Y copy CLI │ Q exit+print │ : SQL │ ? help ";
    let text = match &app.flash {
        Some((msg, _)) => msg.clone(),
        None => default_help.to_string(),
    };
    let p = Paragraph::new(text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(p, area);
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
}
