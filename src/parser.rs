// pq DSL → DuckDB SQL compiler.
//
// ─────────────────────────────────────────────────────────────────────────────
// Grammar (v0.2):
//
//   query        := stage ( '|' stage )*
//                 | raw_sql                       -- starts with SELECT/WITH
//                 | <empty>                       -- => SELECT * LIMIT n
//
//   stage        := projection
//                 | filter_expr
//                 | projection 'where' filter_expr        -- v0 inline shorthand
//                 | 'where'   filter_expr
//                 | 'select'  projection
//                 | 'group_by' '.' ident ( ',' '.' ident )*    -- alias: 'group by'
//                 | 'count'
//                 | ('sum'|'avg'|'min'|'max'|'count_distinct') '.' ident
//                 | 'top' INT 'by' col [ asc | desc ]
//                 | 'sort by' col [ asc | desc ]              -- alias: 'order by'
//                 | 'limit' INT                                -- alias: 'head'
//                 | 'distinct'
//
//   projection   := ('select')? '.' ident ( ',' '.' ident )*
//                 | '.' ident ( '.' ident )*       -- nested struct path
//
//   filter_expr  := <DuckDB SQL fragment>          -- with sugar:
//                     "..."   → '...'  (jq strings to SQL string literals)
//                     ==      → =
//                     !=      → <>
//                     bare .col → col
//
// Source resolution accepts:
//   - local path (foo.parquet, ./data/x.parquet)
//   - glob ('data/dt=2026-*/*.parquet')          -- DuckDB read_parquet handles globs
//   - gs:// / s3:// / az:// / http(s)://         -- via DuckDB httpfs extension
//   - "-" stdin (read from /dev/stdin — needs seekable fd)
// ─────────────────────────────────────────────────────────────────────────────

use anyhow::{anyhow, Result};

use crate::source::InputFormat;

// ─── Source ──────────────────────────────────────────────────────────────────

/// Wrap a path/URI into a DuckDB FROM-clause-friendly source expression
/// for the parquet default. Kept as a thin wrapper for callers that don't
/// know about input formats yet (notably the test suite — every existing
/// snapshot test predates the v0.9 InputFormat plumb).
pub fn source_clause(file: &str) -> String {
    source_clause_fmt(file, InputFormat::Parquet)
}

/// Format-aware source clause. Picks the right DuckDB reader:
///
/// * Parquet: `read_parquet('path' [, hive_partitioning=true])`
/// * NDJSON:  `read_json('path', format='newline_delimited', auto_detect=true)`
/// * CSV:     `read_csv_auto('path')`
///
/// Hive partitioning (`hive_partitioning=true`) auto-enables when the path
/// contains a hive-style segment like `dt=2026-05-21` — this lets the user
/// query partition columns without any flag, e.g.:
///   pq 'sales/dt=2026-*/region=*/*.parquet' 'group_by .dt, .region | count'
///
/// We only auto-detect hive for parquet because the json/csv readers
/// don't accept the option (and partitioned ndjson/csv on disk is a
/// vanishingly rare layout anyway).
pub fn source_clause_fmt(file: &str, fmt: InputFormat) -> String {
    let f = file.trim();
    let escaped = f.replace('\'', "''");
    match fmt {
        InputFormat::Parquet => {
            // Stdin path is special: hive detection makes no sense on
            // `/dev/stdin`, and we trust the caller (source::StdinSpool)
            // to have picked the right physical path.
            if f == "/dev/stdin" || f == "-" {
                return "read_parquet('/dev/stdin')".to_string();
            }
            if looks_like_hive_partition(f) {
                format!("read_parquet('{}', hive_partitioning=true)", escaped)
            } else {
                format!("read_parquet('{}')", escaped)
            }
        }
        InputFormat::Ndjson => format!(
            "read_json('{}', format='newline_delimited', auto_detect=true)",
            escaped
        ),
        InputFormat::Csv => format!("read_csv_auto('{}')", escaped),
    }
}

/// Make a column expression safe to embed inside a SQL alias name.
///
/// We use `<col>` to derive auto-alias names for aggregates (e.g. `sum_amount`).
/// When `<col>` is a qualified name like `b.amount`, the naive concatenation
/// `sum_b.amount` is invalid SQL — DuckDB parses the dot as a struct accessor.
/// Replace any non-identifier character with `_` to keep the alias parseable.
pub(crate) fn alias_safe(col: &str) -> String {
    // v0.11: aggregates over chained-UNNEST paths used to render as
    // `sum_UNNEST_events__amount` because alias_safe just turned every
    // non-identifier byte into `_`. Strip the `UNNEST(<inner>)` wrapper
    // first so the alias reads like the user's original path:
    //   `UNNEST(events).amount` → strip → `events.amount` → `events_amount`
    let stripped = strip_unnest_wrappers(col);
    stripped
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        // collapse runs of `_` (e.g. from `).` after stripping) so we
        // don't end up with double underscores in the alias.
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// Recursively strip `UNNEST(<inner>)` wrappers so e.g.
/// `UNNEST(UNNEST(matrix).row).cell` becomes `matrix.row.cell`. Used by
/// alias generation so chained-unnest aggregates and projections get
/// human-readable column names.
fn strip_unnest_wrappers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let prev_is_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !prev_is_ident && i + 7 <= bytes.len() && s[i..i + 7].eq_ignore_ascii_case("UNNEST(") {
            let open = i + 6;
            if let Some(close) = match_paren(s, open) {
                let inner = &s[open + 1..close];
                out.push_str(&strip_unnest_wrappers(inner));
                i = close + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Given a column expression, return the name DuckDB will assign to the
/// resulting column in a row.
///
/// Examples:
///   `email`                         → `email`
///   `a.email`                       → `email`           (table alias stripped)
///   `sum(revenue) AS sum_revenue`   → `sum_revenue`     (AS alias wins)
///   `lower(.email) AS lower_email`  → `lower_email`
fn final_col_name(expr: &str) -> String {
    let lower = expr.to_ascii_lowercase();
    if let Some(idx) = lower.rfind(" as ") {
        return expr[idx + 4..].trim().to_string();
    }
    expr.rsplit('.').next().unwrap_or(expr).trim().to_string()
}

/// Inverse of `final_col_name` — strip the `AS alias` suffix and return the
/// raw column expression. Used by GROUP BY emission, where DuckDB rejects
/// `expr AS alias` (only the expression OR the alias name is legal there).
/// Case-insensitive on the ` AS ` literal because users sometimes type it
/// in upper case via the raw-SQL escape hatch.
fn strip_as_alias(expr: &str) -> String {
    let lower = expr.to_ascii_lowercase();
    if let Some(idx) = lower.rfind(" as ") {
        expr[..idx].trim().to_string()
    } else {
        expr.trim().to_string()
    }
}

// ─── chained UNNEST hoister (v0.11) ──────────────────────────────────────────
//
// pq's path tokenizer happily emits `UNNEST(events).kind` for `.events[].kind`,
// which reads naturally but DuckDB rejects in two situations that bite real
// queries:
//   1. SELECT list with same-level GROUP BY  → "UNNEST not supported here"
//   2. WHERE/ORDER BY/HAVING                 → same error
// The fix is uniform: lift every `UNNEST(...)` from the outer SELECT into a
// derived FROM subquery that does the row-explosion once, alias each unnest
// expression `_pq_u<i>`, and rewrite the outer fragments to reference those
// aliases. Cost: one extra SELECT layer per query that uses `[]`. Benefit:
// `.events[].kind` works in projection, where, group_by, sort_by uniformly.
//
// We dedupe on the inner expression so `.events[].kind` and `.events[].amount`
// share `_pq_u0` rather than producing two independent unnests of the same
// list (which would multiply rows incorrectly).

/// Find the matching `)` for the `(` at byte index `open` in `s`, ignoring
/// parens inside SQL string literals. Returns `None` if unbalanced.
fn match_paren(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 1usize;
    let mut i = open + 1;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_double && b == b'\'' {
            in_single = !in_single;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
        }
        if !in_single && !in_double {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Walk `fragment` and replace every chained `UNNEST(<expr>)` (one followed
/// by `.`, `[`, or sitting in a context that needs lifting) with an alias
/// drawn from / appended to `sources`. Bare terminal `UNNEST(<expr>)` (i.e.
/// the entire SELECT item is `UNNEST(events)` with nothing after) is left
/// alone — that form already works in DuckDB and rewriting it churns the
/// SQL diff on every test.
///
/// `force_lift` overrides the "only when chained" rule for clauses where
/// terminal UNNEST is also illegal — namely WHERE / GROUP BY / HAVING /
/// ORDER BY. (DuckDB allows `SELECT UNNEST(x)` but not
/// `WHERE UNNEST(x) IS NOT NULL`.)
fn lift_unnest(fragment: &str, sources: &mut Vec<(String, String)>, force_lift: bool) -> String {
    let bytes = fragment.as_bytes();
    let mut out = String::with_capacity(fragment.len());
    let mut i = 0;
    while i < bytes.len() {
        // Match "UNNEST(" case-insensitively, but only when the preceding
        // char isn't an identifier byte (so we don't munge `MY_UNNEST(`).
        let prev_is_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let starts_unnest = i + 7 <= bytes.len()
            && fragment[i..i + 7].eq_ignore_ascii_case("UNNEST(")
            && !prev_is_ident;
        if starts_unnest {
            let open = i + 6;
            if let Some(close) = match_paren(fragment, open) {
                let inner = fragment[open + 1..close].trim().to_string();
                let next = bytes.get(close + 1).copied();
                let chained = matches!(next, Some(b'.') | Some(b'['));
                if chained || force_lift {
                    let alias = match sources.iter().find(|(e, _)| e == &inner) {
                        Some((_, a)) => a.clone(),
                        None => {
                            let a = format!("_pq_u{}", sources.len());
                            sources.push((inner, a.clone()));
                            a
                        }
                    };
                    out.push_str(&alias);
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Heuristic: does the path contain at least one `name=value` directory segment?
/// We deliberately scan just the path bytes — works for local paths, gs://, s3://.
fn looks_like_hive_partition(path: &str) -> bool {
    for segment in path.split('/') {
        if let Some(eq_idx) = segment.find('=') {
            // Must have an identifier-ish key on the left and SOMETHING on the right.
            let key = &segment[..eq_idx];
            let val = &segment[eq_idx + 1..];
            if !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && !val.is_empty()
            {
                return true;
            }
        }
    }
    false
}

// ─── Plan ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aggregate {
    Count,
    CountDistinct(String),
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

impl Aggregate {
    fn sql(&self) -> String {
        match self {
            Aggregate::Count => "count(*) AS count".into(),
            Aggregate::CountDistinct(c) => {
                format!("count(DISTINCT {c}) AS count_distinct_{}", alias_safe(c))
            }
            Aggregate::Sum(c) => format!("sum({c}) AS sum_{}", alias_safe(c)),
            Aggregate::Avg(c) => format!("avg({c}) AS avg_{}", alias_safe(c)),
            Aggregate::Min(c) => format!("min({c}) AS min_{}", alias_safe(c)),
            Aggregate::Max(c) => format!("max({c}) AS max_{}", alias_safe(c)),
        }
    }

    /// The bare alias ("count", "sum_revenue", ...) — what the row's column will
    /// be called once DuckDB has executed the aggregate. Used by line-output
    /// stages to reference inner columns from the outer wrapping SELECT.
    fn alias(&self) -> String {
        match self {
            Aggregate::Count => "count".into(),
            Aggregate::CountDistinct(c) => format!("count_distinct_{}", alias_safe(c)),
            Aggregate::Sum(c) => format!("sum_{}", alias_safe(c)),
            Aggregate::Avg(c) => format!("avg_{}", alias_safe(c)),
            Aggregate::Min(c) => format!("min_{}", alias_safe(c)),
            Aggregate::Max(c) => format!("max_{}", alias_safe(c)),
        }
    }
}

/// SQL JOIN strength. Maps directly onto DuckDB's join keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

impl JoinKind {
    fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT OUTER JOIN",
            JoinKind::Right => "RIGHT OUTER JOIN",
            JoinKind::Full => "FULL OUTER JOIN",
        }
    }
}

/// Single equi-join. Reference left/right sides as `.a.col` / `.b.col` in
/// subsequent stages.
///
/// `on_expr` is a complete SQL ON clause body (e.g. `a.id = b.user_id`), already
/// rewritten from the user's DSL form. Three input shapes (parsed in this order):
///   `on .col`                            ─ shorthand for `a.col = b.col`
///   `on .a.id == .b.user_id`             ─ single key, explicit (must contain `=`)
///   `on .a.x == .b.x and .a.y == .b.y`   ─ multi-key (just compose with `and`/`AND`)
#[derive(Debug, Clone)]
pub struct Join {
    pub kind: JoinKind,
    pub right_source: String,
    pub on_expr: String,
}

/// Per-row line output. Folds the projection list down to a single TEXT column
/// that the renderer prints raw, one row per line — no quoting, no header.
/// Useful when piping into `jq`, `awk`, `xsv`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineFormat {
    /// `concat_ws(',', col1::TEXT, col2::TEXT, ...)` — minimal CSV (no quoting).
    Csv,
    /// `to_json({col1: col1, col2: col2, ...})` — DuckDB's struct-to-JSON.
    Json,
}

#[derive(Debug, Default)]
pub struct QueryPlan {
    pub source: String,           // "read_parquet('...')"
    pub join: Option<Join>,       // single equi-join (left = `a`, right = `b`)
    pub projections: Vec<String>, // explicit SELECT columns (non-aggregate)
    pub aggregates: Vec<Aggregate>,
    pub filters: Vec<String>, // WHERE
    pub group_by: Vec<String>,
    pub havings: Vec<String>, // HAVING (filters that arrive after a grouping verb)
    pub order_by: Vec<(String, bool)>, // (col_or_expr, ascending)
    pub limit: Option<usize>,
    pub distinct: bool,
    /// When set, fold the SELECT list into a single TEXT column and switch the
    /// output format to raw-lines (one line per row, no header).
    pub line_format: Option<LineFormat>,
}

/// The compile() output. We return more than just SQL because some stages
/// (`to_csv`, `to_json`) need to flag the renderer to emit raw lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileOutput {
    pub sql: String,
    /// True iff the query produces a single TEXT column meant to be printed as-is.
    pub raw_lines: bool,
}

impl QueryPlan {
    fn new(source: String) -> Self {
        Self {
            source,
            ..Default::default()
        }
    }

    fn has_grouping(&self) -> bool {
        !self.aggregates.is_empty() || !self.group_by.is_empty()
    }

    /// Final column NAMES (i.e. how rows are addressed *after* the inner SELECT
    /// runs). Used by line-output wrapping to build the outer column refs.
    /// Returns an empty Vec if the projection is `*` — caller decides whether
    /// that's an error or means "all columns".
    fn final_column_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.has_grouping() {
            for g in &self.group_by {
                out.push(final_col_name(g));
            }
            for a in &self.aggregates {
                out.push(a.alias());
            }
        } else if !self.projections.is_empty() {
            for p in &self.projections {
                out.push(final_col_name(p));
            }
        }
        out
    }

    pub fn to_sql(&self) -> String {
        let core = self.to_sql_core();
        match self.line_format {
            None => core,
            Some(fmt) => self.wrap_for_line_output(core, fmt),
        }
    }

    /// SQL for the underlying data pipeline (projections, joins, filters,
    /// grouping, ordering, limit) — without any line-format wrapping.
    ///
    /// v0.11: every fragment runs through `lift_unnest`, which collects
    /// chained-UNNEST expressions into a shared `unnest_sources` list and
    /// rewrites them to reference an alias `_pq_u<i>`. If anything got
    /// lifted, the FROM clause becomes a derived table that does the
    /// unnesting once. This is the single place where chained `[]` becomes
    /// legal DuckDB.
    fn to_sql_core(&self) -> String {
        let mut unnest_sources: Vec<(String, String)> = Vec::new();

        // SELECT list — UNNEST in projection without grouping is fine for
        // DuckDB *if it's terminal*; chained UNNEST always needs lifting.
        // We only force-lift in clauses where DuckDB rejects every UNNEST.
        let lifted_projections: Vec<String> = self
            .projections
            .iter()
            .map(|p| lift_unnest(p, &mut unnest_sources, false))
            .collect();
        let lifted_aggregates: Vec<String> = self
            .aggregates
            .iter()
            .map(|a| lift_unnest(&a.sql(), &mut unnest_sources, false))
            .collect();
        let lifted_group_by: Vec<String> = self
            .group_by
            .iter()
            .map(|g| lift_unnest(g, &mut unnest_sources, false))
            .collect();
        // WHERE / HAVING / ORDER BY: DuckDB rejects bare UNNEST here too,
        // so force-lift even non-chained occurrences.
        let lifted_filters: Vec<String> = self
            .filters
            .iter()
            .map(|f| lift_unnest(f, &mut unnest_sources, true))
            .collect();
        let lifted_havings: Vec<String> = self
            .havings
            .iter()
            .map(|h| lift_unnest(h, &mut unnest_sources, true))
            .collect();
        let lifted_order_by: Vec<(String, bool)> = self
            .order_by
            .iter()
            .map(|(c, asc)| (lift_unnest(c, &mut unnest_sources, true), *asc))
            .collect();

        // Build the FROM expression. If anything was lifted, wrap the
        // original source (and any join) in a derived table that does the
        // unnesting once. We re-export everything from the inner source
        // via `*` plus the new alias columns, so outer references like
        // `country` and `_pq_u0.kind` both resolve.
        let from_expr = self.build_from_clause();
        let final_from = if unnest_sources.is_empty() {
            from_expr
        } else {
            let unnest_cols: Vec<String> = unnest_sources
                .iter()
                .map(|(expr, alias)| format!("UNNEST({}) AS {}", expr, alias))
                .collect();
            format!(
                "(SELECT *, {} FROM {}) AS _pq_src",
                unnest_cols.join(", "),
                from_expr
            )
        };

        // Rebuild the SELECT list from lifted fragments (mirrors
        // `select_list()` but uses the lifted vectors so we don't double-
        // recompute).
        let select_list = if self.has_grouping() {
            let mut parts: Vec<String> = lifted_group_by.clone();
            parts.extend(lifted_aggregates);
            if parts.is_empty() {
                "*".into()
            } else {
                parts.join(", ")
            }
        } else if !lifted_projections.is_empty() {
            lifted_projections.join(", ")
        } else {
            "*".into()
        };

        let mut sql = String::with_capacity(128);
        sql.push_str("SELECT ");
        if self.distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&select_list);
        sql.push_str(" FROM ");
        sql.push_str(&final_from);

        if !lifted_filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&lifted_filters.join(" AND "));
        }
        if !lifted_group_by.is_empty() {
            // GROUP BY can take either the bare expression OR the alias,
            // but NOT `expr AS alias` — that's a parse error in DuckDB.
            sql.push_str(" GROUP BY ");
            let bare: Vec<String> = lifted_group_by.iter().map(|g| strip_as_alias(g)).collect();
            sql.push_str(&bare.join(", "));
        }
        if !lifted_havings.is_empty() {
            sql.push_str(" HAVING ");
            sql.push_str(&lifted_havings.join(" AND "));
        }
        if !lifted_order_by.is_empty() {
            sql.push_str(" ORDER BY ");
            let parts: Vec<String> = lifted_order_by
                .iter()
                .map(|(c, asc)| format!("{} {}", c, if *asc { "ASC" } else { "DESC" }))
                .collect();
            sql.push_str(&parts.join(", "));
        }
        if let Some(n) = self.limit {
            sql.push_str(&format!(" LIMIT {n}"));
        }
        sql
    }

    /// The `<source> [AS a JOIN <right> AS b ON ...]` portion. Pulled out
    /// so `to_sql_core` can wrap it in a derived table when chained UNNEST
    /// hoisting kicks in. (Named `build_from_clause` rather than
    /// `from_clause` to dodge clippy's `wrong_self_convention` rule, which
    /// reads `from_*` as constructor-only.)
    fn build_from_clause(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.source);
        if let Some(j) = &self.join {
            s.push_str(" AS a ");
            s.push_str(j.kind.sql());
            s.push(' ');
            s.push_str(&j.right_source);
            s.push_str(" AS b ON ");
            s.push_str(&j.on_expr);
        }
        s
    }

    /// Wrap the inner pipeline in an outer SELECT that collapses each row to a
    /// single TEXT column (`line`). Done as a subquery so that ORDER BY in the
    /// inner can still reference aliases / aggregates that we no longer
    /// surface from the outer SELECT.
    fn wrap_for_line_output(&self, inner_sql: String, fmt: LineFormat) -> String {
        let cols = self.final_column_names();
        let collapse = match (fmt, cols.is_empty()) {
            // No explicit projections + JSON → struct-of-row via DuckDB's
            // struct_pack on the table alias.
            (LineFormat::Json, true) => "to_json(__pq_inner) AS line".to_string(),
            // No explicit projections + CSV → can't enumerate columns at
            // compile time. Emit invalid SQL (`<NULL>`) so DuckDB's error
            // message tells the user what's wrong; the alternative would be
            // to make to_sql() fallible just for this edge case.
            (LineFormat::Csv, true) => {
                "/* to_csv requires an explicit projection (e.g. .col1, .col2 | to_csv) */ NULL AS line".to_string()
            }
            (LineFormat::Csv, false) => {
                let casts: Vec<String> = cols.iter().map(|c| format!("{c}::TEXT")).collect();
                format!("concat_ws(',', {}) AS line", casts.join(", "))
            }
            (LineFormat::Json, false) => {
                let pairs: Vec<String> = cols.iter().map(|c| format!("\"{c}\": {c}")).collect();
                format!("to_json({{{}}}) AS line", pairs.join(", "))
            }
        };
        format!("SELECT {collapse} FROM ({inner_sql}) AS __pq_inner")
    }
}

// ─── compile_plan() ──────────────────────────────────────────────────────────

/// Compile a pq DSL query to DuckDB SQL plus a "raw lines" hint flag for
/// the renderer. Defaults to parquet input — the historical behaviour.
/// Use `compile_plan_fmt` to feed in ndjson/csv stdin chains (v0.9).
///
/// The wrapper has no production callers as of v0.11 (everything routes
/// through `compile_plan_fmt`) but the parser test suite still leans on
/// the parquet-default shorthand — keeping the function avoids touching
/// dozens of `cmp(...)` test helpers for zero behavior change.
#[allow(dead_code)]
pub fn compile_plan(file: &str, query: &str, default_limit: usize) -> Result<CompileOutput> {
    compile_plan_fmt(file, query, default_limit, InputFormat::Parquet)
}

/// Format-aware compile. `fmt` selects the DuckDB `read_*` table function
/// for the source. Joins still default to parquet for the right-hand side
/// (mixed-format joins are an edge case we don't try to model in v0.9).
pub fn compile_plan_fmt(
    file: &str,
    query: &str,
    default_limit: usize,
    fmt: InputFormat,
) -> Result<CompileOutput> {
    let src = source_clause_fmt(file, fmt);
    let q = query.trim();

    if q.is_empty() {
        let limit = if default_limit == 0 {
            String::new()
        } else {
            format!(" LIMIT {default_limit}")
        };
        return Ok(CompileOutput {
            sql: format!("SELECT * FROM {src}{limit}"),
            raw_lines: false,
        });
    }

    // Raw SQL passthrough (escape hatch). FILE → read_parquet('...').
    // We accept SELECT/WITH (the standard DML escape hatch) plus EXPLAIN/
    // PRAGMA so users can inspect plans and tweak DuckDB session state
    // without needing the duckdb CLI installed.
    let lower_first = q.chars().take(10).collect::<String>().to_ascii_lowercase();
    if lower_first.starts_with("select ")
        || lower_first.starts_with("with ")
        || lower_first.starts_with("explain ")
        || lower_first.starts_with("pragma ")
    {
        return Ok(CompileOutput {
            sql: q.replace("FILE", &src),
            raw_lines: false,
        });
    }

    let mut plan = QueryPlan::new(src);
    for stage in split_pipe_stages(q) {
        parse_stage(stage, &mut plan)?;
    }
    let raw_lines = plan.line_format.is_some();
    Ok(CompileOutput {
        sql: plan.to_sql(),
        raw_lines,
    })
}

// ─── pipe-stage parsing ──────────────────────────────────────────────────────

/// Split a query on top-level `|`, honoring quoted strings.
fn split_pipe_stages(q: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = q.as_bytes();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        } else if c == '|' && !in_single && !in_double {
            out.push(q[start..i].trim());
            start = i + 1;
        }
    }
    out.push(q[start..].trim());
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

fn parse_stage(stage: &str, plan: &mut QueryPlan) -> Result<()> {
    let s = stage.trim();
    if s.is_empty() {
        return Err(anyhow!("empty stage"));
    }
    let lower = s.to_ascii_lowercase();

    // group_by .col[, .col]   (also: 'group by')
    if let Some(rest) = strip_keyword(s, "group_by").or_else(|| strip_keyword(s, "group by")) {
        let cols = parse_projection(rest)?;
        plan.group_by
            .extend(cols.split(", ").map(|c| c.trim().to_string()));
        return Ok(());
    }

    // where EXPR  → WHERE if no grouping yet, HAVING if grouping already declared.
    if let Some(rest) = strip_keyword(s, "where") {
        let expr = rewrite_filter(rest);
        if plan.has_grouping() {
            plan.havings.push(expr);
        } else {
            plan.filters.push(expr);
        }
        return Ok(());
    }

    // count (bare)
    if lower == "count" || lower == "count(*)" {
        plan.aggregates.push(Aggregate::Count);
        return Ok(());
    }

    // count_distinct .col / sum .col / avg / min / max
    type AggCtor = fn(String) -> Aggregate;
    for (kw, ctor) in [
        ("count_distinct", Aggregate::CountDistinct as AggCtor),
        ("sum", Aggregate::Sum),
        ("avg", Aggregate::Avg),
        ("min", Aggregate::Min),
        ("max", Aggregate::Max),
    ] {
        if let Some(rest) = strip_keyword(s, kw) {
            let raw = rest.trim();
            if raw.is_empty() {
                return Err(anyhow!(
                    "`{}` requires a column (e.g. `{} .revenue`)",
                    kw,
                    kw
                ));
            }
            // v0.10: aggregate target can be a nested path —
            // `sum .events[0].amount`, `avg .user.age`. Falls back to
            // `strip_dot` so plain `sum .revenue` still compiles.
            let col = if raw.starts_with('.') {
                path_to_sql(raw).unwrap_or_else(|_| strip_dot(raw))
            } else {
                strip_dot(raw)
            };
            plan.aggregates.push(ctor(col));
            return Ok(());
        }
    }

    // top N by COL [asc|desc]    (defaults to DESC)
    if let Some(rest) = strip_keyword(s, "top") {
        let (n_str, by_part) = rest
            .split_once(" by ")
            .ok_or_else(|| anyhow!("`top` needs `by COL`: `top N by .col`"))?;
        let n: usize = n_str
            .trim()
            .parse()
            .map_err(|_| anyhow!("`top` needs an integer, got `{}`", n_str.trim()))?;
        let (col, asc) = parse_orderby_clause(by_part, false /* top default = DESC */);
        plan.order_by.push((col, asc));
        plan.limit = Some(n);
        return Ok(());
    }

    // sort by / order by  (defaults to ASC)
    if let Some(rest) = strip_keyword(s, "sort by").or_else(|| strip_keyword(s, "order by")) {
        let (col, asc) = parse_orderby_clause(rest, true /* default = ASC */);
        plan.order_by.push((col, asc));
        return Ok(());
    }

    // limit N / head N — also accept the unix flag forms `head -n 5`
    // and `head -5` because shell-pipe muscle memory will reach for
    // them (the error message used to say "got `-n 3`" which felt
    // hostile when the user is just typing what `head` always took).
    if let Some(rest) = strip_keyword(s, "limit").or_else(|| strip_keyword(s, "head")) {
        let arg = rest.trim();
        let cleaned = arg
            .strip_prefix("-n")
            .map(|x| x.trim())
            .or_else(|| arg.strip_prefix('-'))
            .unwrap_or(arg)
            .trim();
        let n: usize = cleaned.parse().map_err(|_| {
            anyhow!(
                "`limit/head` needs a non-negative integer (e.g. `head 5` or `limit 5`), got `{}`",
                arg
            )
        })?;
        plan.limit = Some(n);
        return Ok(());
    }

    // distinct (bare)
    if lower == "distinct" {
        plan.distinct = true;
        return Ok(());
    }

    // Joins.
    //   `join` / `inner_join`              → INNER JOIN
    //   `left_join`  / `left join`         → LEFT OUTER JOIN
    //   `right_join` / `right join`        → RIGHT OUTER JOIN
    //   `full_join`  / `full join` /
    //   `outer_join` / `outer join`        → FULL OUTER JOIN
    //
    // ON-clause shapes (auto-detected by presence of `=`):
    //   `on .col`                          → a.col = b.col
    //   `on .a.x == .b.y`                  → a.x = b.y
    //   `on .a.x == .b.x and .a.y == .b.y` → a.x = b.x AND a.y = b.y     (multi-key)
    //
    // Match the more-specific verbs first; `join` is a strict prefix that would
    // otherwise capture e.g. `join_filter` (we still use strip_keyword which
    // requires a whitespace boundary, so the order is defensive only).
    let join_match: Option<(JoinKind, &str)> =
        if let Some(r) = strip_keyword(s, "left_join").or_else(|| strip_keyword(s, "left join")) {
            Some((JoinKind::Left, r))
        } else if let Some(r) =
            strip_keyword(s, "right_join").or_else(|| strip_keyword(s, "right join"))
        {
            Some((JoinKind::Right, r))
        } else if let Some(r) = strip_keyword(s, "full_join")
            .or_else(|| strip_keyword(s, "full join"))
            .or_else(|| strip_keyword(s, "outer_join"))
            .or_else(|| strip_keyword(s, "outer join"))
        {
            Some((JoinKind::Full, r))
        } else if let Some(r) = strip_keyword(s, "inner_join")
            .or_else(|| strip_keyword(s, "inner join"))
            .or_else(|| strip_keyword(s, "join"))
        {
            Some((JoinKind::Inner, r))
        } else {
            None
        };

    if let Some((kind, rest)) = join_match {
        if plan.join.is_some() {
            return Err(anyhow!(
                "multiple joins are not supported yet; fall back to raw SQL via SELECT/WITH for chained joins"
            ));
        }
        let (path_raw, on_part) = rest.split_once(" on ").ok_or_else(|| {
            anyhow!(
                "`{} ...` needs `on .col`: `{} \"file.parquet\" on .user_id`",
                kind.sql(),
                kind.sql()
            )
        })?;
        let path = path_raw.trim().trim_matches('"').trim_matches('\'').trim();
        if path.is_empty() {
            return Err(anyhow!("`join` needs a quoted file path"));
        }
        let right_source = source_clause(path);
        let on_expr = if on_part.contains('=') {
            // Multi-key `and` chains pass through unchanged because rewrite_filter
            // only touches comparison operators / quotes / dot-prefixes; `and`/`AND`
            // are valid SQL keywords already.
            let rewritten = rewrite_filter(on_part);
            let trimmed = rewritten.trim().to_string();
            if trimmed.is_empty() {
                return Err(anyhow!("`join on` clause is empty"));
            }
            trimmed
        } else {
            let col = strip_dot(on_part);
            if col.is_empty() {
                return Err(anyhow!("`join` needs a column name after `on`"));
            }
            format!("a.{col} = b.{col}")
        };
        plan.join = Some(Join {
            kind,
            right_source,
            on_expr,
        });
        return Ok(());
    }

    // Per-row line output stages: bare `to_csv` / `to_json` (no args).
    // These must be the LAST stage in a query — anything after is silently
    // ignored (well, parsed but the SELECT list collapse happens regardless).
    //
    // Aliases (v0.9.1):
    // * `to_tsv` → tab-less CSV is rare in practice; intentionally NOT an alias
    //   for to_csv (different separator), keep them distinct.
    // * `to_ndjson` / `to_jsonl` → both are unix-world names for what pq's
    //   `to_json` already emits (one JSON object per row, newline-delimited).
    //   Added because `to_ndjson` is what users intuitively reach for when
    //   chaining `pq | jq | pq -i ndjson -`.
    if lower == "to_csv" || lower == "tocsv" {
        plan.line_format = Some(LineFormat::Csv);
        return Ok(());
    }
    if lower == "to_json" || lower == "tojson" || lower == "to_ndjson" || lower == "to_jsonl" {
        plan.line_format = Some(LineFormat::Json);
        return Ok(());
    }

    // explicit `select .a, .b`
    if let Some(rest) = strip_keyword(s, "select") {
        let cols = parse_projection(rest)?;
        plan.projections
            .extend(cols.split(", ").map(|c| c.trim().to_string()));
        return Ok(());
    }

    // Bare projection or projection-with-inline-where (v0 syntax).
    //
    // Triggered when the stage starts with `.` (jq-style path) OR when the
    // stage is shaped as a recognised path-function call: `len(.events)`,
    // `keys(.metadata)`, `values(.metadata)`, `length(.tags)`. Without the
    // path-function arm a bare `keys(.metadata)` would fall through to the
    // "last resort" filter branch and emit `WHERE keys(metadata)` — DuckDB
    // tries to evaluate that as a boolean and explodes with "Catalog Error:
    // Scalar Function with name keys does not exist", because the WHERE
    // path doesn't go through `rewrite_path_function`. Routing the same
    // syntax through projection lets parse_projection do the rewrite.
    let path_fn_proj = rewrite_path_function(s).is_some()
        || split_inline_where(s)
            .map(|(p, _)| rewrite_path_function(p).is_some())
            .unwrap_or(false);
    if s.starts_with('.') || path_fn_proj {
        if let Some((proj_part, filter_part)) = split_inline_where(s) {
            let cols = parse_projection(proj_part)?;
            plan.projections
                .extend(cols.split(", ").map(|c| c.trim().to_string()));
            let f = rewrite_filter(filter_part);
            if plan.has_grouping() {
                plan.havings.push(f);
            } else {
                plan.filters.push(f);
            }
        } else {
            let cols = parse_projection(s)?;
            plan.projections
                .extend(cols.split(", ").map(|c| c.trim().to_string()));
        }
        return Ok(());
    }

    // Last resort: bare filter expression.
    let f = rewrite_filter(s);
    if plan.has_grouping() {
        plan.havings.push(f);
    } else {
        plan.filters.push(f);
    }
    Ok(())
}

// ─── small helpers ───────────────────────────────────────────────────────────

/// If `s` (case-insensitive) starts with `keyword` followed by whitespace,
/// return the trimmed remainder. Bare keyword without args returns None.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let kw_lower = keyword.to_ascii_lowercase();
    let s_lower = s.to_ascii_lowercase();
    let needle = format!("{} ", kw_lower);
    if s_lower.starts_with(&needle) {
        Some(s[needle.len()..].trim())
    } else if s_lower == kw_lower {
        Some("") // bare keyword, no args; callers decide if that's valid
    } else {
        None
    }
}

fn strip_dot(s: &str) -> String {
    s.trim().trim_start_matches('.').trim().to_string()
}

// ─── Path tokenizer (v0.10) ──────────────────────────────────────────────────
//
// jq-style path syntax with bracket sugar for nested types:
//
//   .foo                  → foo                     plain identifier
//   .foo.bar              → foo.bar                 STRUCT field access
//   .foo[0]               → foo[1]                  LIST index (jq 0-idx → DuckDB 1-idx)
//   .foo[-1]              → foo[-1]                 last element (DuckDB native)
//   .foo[]                → UNNEST(foo)             LIST explosion (projection only)
//   .foo["plan"]          → element_at(foo,'plan')[1]  MAP value access
//   .foo['plan']          → element_at(foo,'plan')[1]  same with single quotes
//   .foo.bar[0]           → foo.bar[1]              chained
//   .foo[0].bar           → foo[1].bar              index-then-struct
//
// Limitations (v0.10): bare `[]` is only meaningful as a top-level
// projection — `.events[].kind` requires re-stating as a CTE / raw SQL.
// We could add list_transform desugaring later, but the projection case
// covers ~80% of analyst use.
//
// We intentionally implement this as a hand-rolled tokenizer rather than
// reaching for a parser combinator crate — the grammar is small enough
// that explicit code is easier to read and audit.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSegment {
    /// Plain identifier or struct field. `head` distinguishes the first
    /// segment (no leading `.` in SQL) from `.bar` continuations.
    Field(String),
    /// `[N]` — DuckDB list index. Stored already 1-converted from jq's
    /// 0-indexed input. Negative indices pass through unchanged.
    Index(i64),
    /// `[]` — list explosion. Must be the LAST segment.
    Unnest,
    /// `["k"]` / `['k']` — MAP key lookup.
    MapKey(String),
}

/// Translate a single pq DSL path expression into DuckDB SQL.
pub(crate) fn path_to_sql(path: &str) -> Result<String> {
    let segments = tokenize_path(path)?;
    render_path(&segments)
}

/// Generate a clean SQL alias for a tokenized path. DuckDB's auto-naming
/// would emit `(events[1]).amount` or `element_at(metadata, 'plan')[1]` —
/// fine internally, hostile in JSON output and downstream chains. We
/// derive a snake_case alias from the path segments so users see
/// `events_0_amount` / `metadata_plan` instead.
///
/// Only fires when the path contains brackets — plain struct dot paths
/// (`.user.email`) keep DuckDB's default auto-naming (`email`) so we
/// don't break the historical contract that `pq f '.user.email'`
/// produces a JSON key called `email`.
pub(crate) fn alias_for_path(path: &str) -> Option<String> {
    let segments = tokenize_path(path).ok()?;
    if segments.len() == 1 {
        // Plain `.foo` — DuckDB names it `foo`, which is already clean.
        return None;
    }
    // No brackets → pure struct path, defer to DuckDB's auto-naming for
    // backward compat (`.user.email` → `email`, not `user_email`).
    let has_bracket = segments.iter().any(|s| {
        matches!(
            s,
            PathSegment::Index(_) | PathSegment::MapKey(_) | PathSegment::Unnest
        )
    });
    if !has_bracket {
        return None;
    }
    let mut parts: Vec<String> = Vec::with_capacity(segments.len());
    for seg in &segments {
        match seg {
            PathSegment::Field(name) => parts.push(name.clone()),
            PathSegment::Index(n) => {
                // Translate back to jq's 0-indexed form for human-friendly
                // aliases. `tags[1]` (DuckDB) was `.tags[0]` (jq) — surface
                // the latter spelling.
                let display = if *n > 0 { *n - 1 } else { *n };
                parts.push(if display < 0 {
                    format!("neg{}", -display)
                } else {
                    display.to_string()
                });
            }
            PathSegment::MapKey(k) => parts.push(alias_safe(k)),
            PathSegment::Unnest => {
                // `.tags[]` → alias `tags`. UNNEST exploded rows already
                // surface as the flat element type.
            }
        }
    }
    let joined = parts.join("_");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn tokenize_path(path: &str) -> Result<Vec<PathSegment>> {
    let s = path.trim();
    if s.is_empty() {
        return Err(anyhow!("empty path"));
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut out: Vec<PathSegment> = Vec::new();

    // Optional leading dot — `.foo` or `foo` are both accepted; the latter
    // matters for places like `sort by foo` where users sometimes drop the
    // dot out of habit.
    if bytes.first() == Some(&b'.') {
        i = 1;
    }

    // First segment must be a bare identifier (no leading `.` in SQL output).
    let (first, next) = read_ident(bytes, i)?;
    out.push(PathSegment::Field(first));
    i = next;

    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                let (ident, next) = read_ident(bytes, i + 1)?;
                out.push(PathSegment::Field(ident));
                i = next;
            }
            b'[' => {
                let (seg, next) = read_bracket(bytes, i)?;
                out.push(seg);
                i = next;
            }
            other => {
                return Err(anyhow!(
                    "unexpected character '{}' in path '{}'",
                    other as char,
                    path
                ));
            }
        }
    }

    // v0.11 allows `[]` to appear mid-path. The lifter in `to_sql_core`
    // hoists every `UNNEST(...)` (whether terminal or chained) into a
    // derived FROM subquery so DuckDB never sees `UNNEST` in a context
    // where it complains ("UNNEST not supported here"). Tokenizer no
    // longer needs to enforce terminal-only.

    Ok(out)
}

fn read_ident(bytes: &[u8], start: usize) -> Result<(String, usize)> {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    if i == start {
        return Err(anyhow!(
            "expected an identifier at offset {start}, got {:?}",
            bytes.get(start).map(|&b| b as char)
        ));
    }
    Ok((
        std::str::from_utf8(&bytes[start..i])
            .map_err(|e| anyhow!("invalid utf-8 in path: {}", e))?
            .to_string(),
        i,
    ))
}

fn read_bracket(bytes: &[u8], start: usize) -> Result<(PathSegment, usize)> {
    debug_assert_eq!(bytes[start], b'[');
    let close = start
        + 1
        + bytes[start + 1..]
            .iter()
            .position(|&b| b == b']')
            .ok_or_else(|| anyhow!("unterminated '[' in path"))?;
    let inner = std::str::from_utf8(&bytes[start + 1..close])
        .map_err(|e| anyhow!("invalid utf-8 inside brackets: {}", e))?
        .trim();

    // []  (bare) → UNNEST
    if inner.is_empty() {
        return Ok((PathSegment::Unnest, close + 1));
    }
    // ["str"] / ['str'] → map-key access. We re-quote to single quotes for
    // SQL and escape any embedded single quotes.
    if (inner.starts_with('"') && inner.ends_with('"'))
        || (inner.starts_with('\'') && inner.ends_with('\''))
    {
        let key = &inner[1..inner.len() - 1];
        return Ok((PathSegment::MapKey(key.to_string()), close + 1));
    }
    // [N] / [-N] → list index. jq is 0-indexed, DuckDB is 1-indexed; convert
    // positives by +1, leave negatives alone (DuckDB supports `lst[-1]`
    // for the last element directly).
    if let Ok(n) = inner.parse::<i64>() {
        let translated = if n >= 0 { n + 1 } else { n };
        return Ok((PathSegment::Index(translated), close + 1));
    }
    Err(anyhow!("can't parse bracket subscript [{}]", inner))
}

fn render_path(segments: &[PathSegment]) -> Result<String> {
    if segments.is_empty() {
        return Err(anyhow!("empty path"));
    }
    // Build incrementally, applying UNNEST as a wrapper at the end.
    let mut sql = match &segments[0] {
        PathSegment::Field(name) => name.clone(),
        other => {
            return Err(anyhow!(
                "path must start with an identifier, got {:?}",
                other
            ))
        }
    };
    for seg in &segments[1..] {
        match seg {
            PathSegment::Field(name) => {
                sql.push('.');
                sql.push_str(name);
            }
            PathSegment::Index(n) => {
                sql.push_str(&format!("[{}]", n));
            }
            PathSegment::MapKey(k) => {
                // DuckDB's element_at on a MAP returns a LIST of matching
                // values (because MAP keys aren't required to be unique
                // at the type level). [1] picks the first hit, which
                // matches the user's mental model of "a map lookup".
                let escaped = k.replace('\'', "''");
                sql = format!("element_at({}, '{}')[1]", sql, escaped);
            }
            PathSegment::Unnest => {
                sql = format!("UNNEST({})", sql);
            }
        }
    }
    Ok(sql)
}

/// Mate to `rewrite_path_function`: derive a clean snake_case alias so the
/// JSON output for `len(.tags)` reads `{"len_tags": 3}` instead of the
/// SQL-flavoured `{"len(tags)": 3}`.
pub(crate) fn path_function_alias(expr: &str) -> Option<String> {
    let s = expr.trim();
    let lower = s.to_ascii_lowercase();
    for (head, alias_prefix) in [
        ("len(", "len"),
        ("length(", "length"),
        ("keys(", "keys"),
        ("values(", "values"),
    ] {
        if let Some(rest) = lower.strip_prefix(head) {
            if rest.ends_with(')') {
                let arg_start = head.len();
                let arg = s[arg_start..s.len() - 1].trim();
                let arg_alias = if arg.starts_with('.') {
                    alias_for_path(arg).unwrap_or_else(|| arg.trim_start_matches('.').to_string())
                } else {
                    alias_safe(arg)
                };
                return Some(format!("{}_{}", alias_prefix, arg_alias));
            }
        }
    }
    None
}

/// Sugar for the family of jq-style path-aware functions: `len(.foo)`,
/// `length(.foo)`, `keys(.foo)`, `values(.foo)`. Returns the rewritten
/// SQL if the input matches one of them, else None and the caller falls
/// back to the raw expression.
pub(crate) fn rewrite_path_function(expr: &str) -> Option<String> {
    let s = expr.trim();
    let lower = s.to_ascii_lowercase();
    for (head, sql_fn) in [
        ("len(", "len"),
        ("length(", "len"),
        ("keys(", "map_keys"),
        ("values(", "map_values"),
    ] {
        if let Some(rest) = lower.strip_prefix(head) {
            if let Some(rest) = rest.strip_suffix(')') {
                let arg_start = head.len();
                let arg = &s[arg_start..s.len() - 1];
                let arg = arg.trim();
                if let Ok(p) = path_to_sql(arg) {
                    return Some(format!("{}({})", sql_fn, p));
                }
                // length on a literal — let DuckDB handle it as-is.
                let _ = rest;
                return Some(format!("{}({})", sql_fn, arg));
            }
        }
    }
    None
}

/// Split "col" / ".col" / "col asc" / ".col desc" — returns (col_sql, ascending).
/// Path expressions (`.events[0].amount`) are translated through
/// `path_to_sql`; bare identifiers fall through unchanged.
fn parse_orderby_clause(s: &str, default_asc: bool) -> (String, bool) {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (col_part, asc) = if let Some(stripped) = lower.strip_suffix(" desc") {
        (s[..stripped.len()].trim(), false)
    } else if let Some(stripped) = lower.strip_suffix(" asc") {
        (s[..stripped.len()].trim(), true)
    } else {
        (s, default_asc)
    };
    // v0.10: if it starts with a dot, run the full path tokenizer so
    // `sort by .events[0].amount desc` works. Otherwise fall back to
    // strip_dot for backward compatibility with bare-identifier sorts.
    let sql = if col_part.starts_with('.') {
        path_to_sql(col_part).unwrap_or_else(|_| strip_dot(col_part))
    } else {
        strip_dot(col_part)
    };
    (sql, asc)
}

/// Find the FIRST top-level " where " inside a stage (not inside quotes).
/// Returns `(before, after)` if found.
fn split_inline_where(s: &str) -> Option<(&str, &str)> {
    let needle = " where ";
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for i in 0..bytes.len() {
        let c = bytes[i] as char;
        if c == '\'' && !in_double {
            in_single = !in_single;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
        }
        if !in_single && !in_double && lower[i..].starts_with(needle) {
            let before = s[..i].trim();
            let after = s[i + needle.len()..].trim();
            if !after.is_empty() {
                return Some((before, after));
            }
        }
    }
    None
}

/// Parse a projection clause:
///   .a               -> "a"
///   .a, .b           -> "a, b"
///   .user.id         -> "user.id"
///   select .a, .b    -> "a, b"
fn parse_projection(input: &str) -> Result<String> {
    let s = input
        .trim()
        .trim_start_matches("select ")
        .trim_start_matches("SELECT ")
        .trim();

    if s.is_empty() {
        return Err(anyhow!("empty projection"));
    }

    // Split-on-comma is fine here because pq projections don't currently
    // accept function calls with commas (that's what UDF macros are for —
    // their definitions live in --udf, not inline in the projection).
    // Iterate explicitly (rather than .map().collect()) so we can
    // propagate path-tokenizer errors with a `?` — silently swallowing
    // them used to surface as "syntax error at or near ']'" from DuckDB,
    // which never points at the real problem.
    let mut cols: Vec<String> = Vec::new();
    for raw in s.split(',') {
        let c = raw.trim();
        if c.is_empty() {
            continue;
        }
        let translated = if c.starts_with('.') {
            // v0.10: a leading dot triggers full path tokenization so
            // `.foo[]`, `.foo[0]`, `.foo["key"]`, `.foo.bar` all DTRT.
            let sql = path_to_sql(c).map_err(|e| anyhow!("invalid path '{}': {}", c, e))?;
            match alias_for_path(c) {
                Some(alias) => format!("{} AS {}", sql, alias),
                None => sql,
            }
        } else if let Some(rewritten) = rewrite_path_function(c) {
            // `len(.tags)`, `keys(.metadata)` in projection context.
            // Alias these so JSON keys come out as `len_tags` rather
            // than the raw SQL fragment `len(tags)`.
            let alias = path_function_alias(c).unwrap_or_default();
            if alias.is_empty() {
                rewritten
            } else {
                format!("{} AS {}", rewritten, alias)
            }
        } else {
            // Bare expressions (function calls, literals, * etc.) pass
            // through unchanged so users keep their existing escape
            // hatches.
            c.to_string()
        };
        cols.push(translated);
    }

    if cols.is_empty() {
        return Err(anyhow!("could not parse projection: {}", input));
    }

    Ok(cols.join(", "))
}

/// Rewrite a filter expression to DuckDB SQL. See module docstring for sugar rules.
fn rewrite_filter(input: &str) -> String {
    let mut s = String::with_capacity(input.len() + 4);
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];

        if c == '\'' && !in_double {
            in_single = !in_single;
            s.push(c);
            i += 1;
            continue;
        }
        if in_single {
            s.push(c);
            i += 1;
            continue;
        }

        if c == '"' {
            in_double = !in_double;
            s.push('\'');
            i += 1;
            continue;
        }
        if in_double {
            if c == '\'' {
                s.push('\'');
                s.push('\'');
            } else {
                s.push(c);
            }
            i += 1;
            continue;
        }

        if c == '=' && chars.get(i + 1) == Some(&'=') {
            s.push('=');
            i += 2;
            continue;
        }
        if c == '!' && chars.get(i + 1) == Some(&'=') {
            s.push('<');
            s.push('>');
            i += 2;
            continue;
        }
        if c == '.' {
            let next_is_ident = chars
                .get(i + 1)
                .map(|nc| nc.is_ascii_alphabetic() || *nc == '_')
                .unwrap_or(false);
            let prev_is_ident_part = match s.chars().last() {
                Some(p) => p.is_ascii_alphanumeric() || p == '_' || p == ')',
                None => false,
            };
            if next_is_ident && !prev_is_ident_part {
                // v0.10: now that paths can contain `[N]` / `["key"]` /
                // `.bar` segments, snip out the whole token starting at `.`
                // and run it through path_to_sql instead of just dropping
                // the leading dot. Falls back to the historical "drop one
                // dot" behaviour if anything goes wrong, so plain `.foo`
                // queries don't regress.
                let end = scan_path_end(&chars, i);
                let raw: String = chars[i..end].iter().collect();
                match path_to_sql(&raw) {
                    Ok(rewritten) => {
                        // v0.11: UNNEST inside WHERE is now legal — the
                        // hoister in `to_sql_core` rewrites `UNNEST(...)`
                        // into a `_pq_u<i>` alias and lifts the actual
                        // unnest into a derived FROM. So just emit the
                        // path's SQL form here verbatim and let the
                        // lifter handle the rest.
                        s.push_str(&rewritten);
                        i = end;
                        continue;
                    }
                    _ => {
                        // Path tokenizer choked — fall back to the legacy
                        // "drop the dot" behaviour so we don't surprise
                        // existing queries that lean on it.
                        i += 1;
                        continue;
                    }
                }
            }
        }

        s.push(c);
        i += 1;
    }
    s
}

/// Scan from a `.` to the end of a path token (idents, dots, `[...]`).
/// Returns the index *past* the last char of the path. Used when filter
/// rewriting needs to peel off a whole `.foo[0]["k"].bar` token.
fn scan_path_end(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    let mut depth = 0_usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '[' => {
                depth += 1;
                i += 1;
            }
            ']' if depth > 0 => {
                depth -= 1;
                i += 1;
            }
            '.' | '_' => i += 1,
            c if c.is_ascii_alphanumeric() => i += 1,
            // Inside [ ... ] we tolerate quotes / minus / digits — the
            // bracket scanner inside path_to_sql validates the inner
            // content. Outside of brackets we stop here.
            c if depth > 0 && (c == '"' || c == '\'' || c == '-' || c.is_whitespace()) => i += 1,
            _ => break,
        }
    }
    i
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(file: &str, q: &str, n: usize) -> String {
        compile_plan(file, q, n).unwrap().sql
    }

    // ── v0 backward-compat ──────────────────────────────────────────────────

    #[test]
    fn empty_query_default_head() {
        let s = cmp("a.parquet", "", 20);
        assert!(s.contains("SELECT * FROM read_parquet('a.parquet')"));
        assert!(s.ends_with("LIMIT 20"));
    }

    #[test]
    fn projection_simple() {
        assert_eq!(
            cmp("f.parquet", ".email", 20),
            "SELECT email FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn projection_multiple() {
        assert_eq!(
            cmp("f.parquet", ".a, .b, .c", 20),
            "SELECT a, b, c FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn projection_nested() {
        assert_eq!(
            cmp("f.parquet", ".user.id", 20),
            "SELECT user.id FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn filter_only() {
        assert_eq!(
            cmp("f.parquet", "country == \"US\"", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE country = 'US'"
        );
    }

    #[test]
    fn projection_and_filter() {
        assert_eq!(
            cmp("f.parquet", ".email where .country == \"US\"", 20),
            "SELECT email FROM read_parquet('f.parquet') WHERE country = 'US'"
        );
    }

    #[test]
    fn ne_operator_rewrites_to_sql() {
        assert_eq!(
            cmp("f.parquet", ".email where .country != \"US\"", 20),
            "SELECT email FROM read_parquet('f.parquet') WHERE country <> 'US'"
        );
    }

    #[test]
    fn single_quoted_strings_pass_through() {
        assert_eq!(
            cmp("f.parquet", "country = 'US'", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE country = 'US'"
        );
    }

    #[test]
    fn apostrophe_inside_jq_string_gets_escaped() {
        assert_eq!(
            cmp("f.parquet", "name == \"O'Brien\"", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE name = 'O''Brien'"
        );
    }

    #[test]
    fn qualified_column_dot_preserved() {
        assert_eq!(
            cmp("f.parquet", "u.country == \"US\"", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE u.country = 'US'"
        );
    }

    #[test]
    fn cloud_path() {
        assert!(cmp("gs://bucket/file.parquet", "", 20)
            .contains("read_parquet('gs://bucket/file.parquet')"));
    }

    #[test]
    fn raw_sql_passthrough() {
        let s = cmp("ignored.parquet", "SELECT count(*) FROM FILE", 20);
        assert!(s.contains("read_parquet('ignored.parquet')"));
        assert!(s.contains("count(*)"));
    }

    #[test]
    fn count_shortcut() {
        // alias changed from `rows` (v0) to `count` (v0.2 idiomatic).
        let s = cmp("f.parquet", "count", 20);
        assert!(s.contains("count(*) AS count"));
        assert!(s.contains("read_parquet('f.parquet')"));
    }

    // ── v0.2 pipe-stage ─────────────────────────────────────────────────────

    #[test]
    fn group_by_count() {
        assert_eq!(
            cmp("f.parquet", "group_by .country | count", 20),
            "SELECT country, count(*) AS count FROM read_parquet('f.parquet') GROUP BY country"
        );
    }

    #[test]
    fn group_by_count_legacy_two_word_keyword() {
        assert_eq!(
            cmp("f.parquet", "group by .country | count", 20),
            "SELECT country, count(*) AS count FROM read_parquet('f.parquet') GROUP BY country"
        );
    }

    #[test]
    fn group_by_sum_top_n() {
        // top defaults to DESC. Reference the agg alias `sum_revenue` in `top by`.
        assert_eq!(
            cmp(
                "f.parquet",
                "group_by .country | sum .revenue | top 10 by sum_revenue",
                20
            ),
            "SELECT country, sum(revenue) AS sum_revenue FROM read_parquet('f.parquet') \
             GROUP BY country ORDER BY sum_revenue DESC LIMIT 10"
        );
    }

    #[test]
    fn where_before_group_is_filter_after_is_having() {
        assert_eq!(
            cmp(
                "f.parquet",
                "where .age > 18 | group_by .country | count | where count > 100",
                20
            ),
            "SELECT country, count(*) AS count FROM read_parquet('f.parquet') \
             WHERE age > 18 GROUP BY country HAVING count > 100"
        );
    }

    #[test]
    fn multi_group_by_and_avg() {
        assert_eq!(
            cmp(
                "f.parquet",
                "group_by .country, .age_bucket | avg .revenue",
                20
            ),
            "SELECT country, age_bucket, avg(revenue) AS avg_revenue FROM read_parquet('f.parquet') \
             GROUP BY country, age_bucket"
        );
    }

    #[test]
    fn sort_by_default_asc() {
        assert_eq!(
            cmp("f.parquet", ".email | sort by .email", 20),
            "SELECT email FROM read_parquet('f.parquet') ORDER BY email ASC"
        );
    }

    #[test]
    fn sort_by_desc() {
        assert_eq!(
            cmp(
                "f.parquet",
                ".revenue | sort by .revenue desc | limit 5",
                20
            ),
            "SELECT revenue FROM read_parquet('f.parquet') ORDER BY revenue DESC LIMIT 5"
        );
    }

    #[test]
    fn distinct_stage() {
        assert_eq!(
            cmp("f.parquet", ".country | distinct", 20),
            "SELECT DISTINCT country FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn count_distinct() {
        assert_eq!(
            cmp("f.parquet", "count_distinct .npi", 20),
            "SELECT count(DISTINCT npi) AS count_distinct_npi FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn pipe_inside_quoted_string_is_not_a_separator() {
        // The `|` inside a quoted string must not split stages.
        assert_eq!(
            cmp("f.parquet", "name == \"a|b\"", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE name = 'a|b'"
        );
    }

    #[test]
    fn glob_path_passthrough_with_hive_autodetect() {
        // 'dt=...' segment triggers hive_partitioning=true automatically.
        let s = cmp("data/dt=2026-*/*.parquet", "count", 20);
        assert!(
            s.contains("read_parquet('data/dt=2026-*/*.parquet', hive_partitioning=true)"),
            "got: {}",
            s
        );
        assert!(s.contains("count(*)"));
    }

    #[test]
    fn plain_path_no_hive() {
        let s = cmp("data/2026/05/file.parquet", "count", 20);
        assert!(s.contains("read_parquet('data/2026/05/file.parquet')"));
        assert!(!s.contains("hive_partitioning"));
    }

    #[test]
    fn nested_hive_partitions() {
        let s = cmp("sales/dt=2026-05/region=US/*.parquet", "count", 20);
        assert!(s.contains("hive_partitioning=true"));
    }

    #[test]
    fn hive_path_supports_partition_column_in_query() {
        let s = cmp(
            "sales/dt=2026-*/region=*/*.parquet",
            "group_by .dt, .region | count",
            20,
        );
        // Hive turns dt/region into normal columns DuckDB can group_by.
        assert!(s.contains("GROUP BY dt, region"));
        assert!(s.contains("hive_partitioning=true"));
    }

    #[test]
    fn join_basic() {
        assert_eq!(
            cmp(
                "users.parquet",
                "join \"orders.parquet\" on .user_id | select .a.email, .b.amount",
                20
            ),
            "SELECT a.email, b.amount FROM read_parquet('users.parquet') AS a \
             INNER JOIN read_parquet('orders.parquet') AS b ON a.user_id = b.user_id"
        );
    }

    #[test]
    fn join_with_filter_and_limit() {
        let s = cmp(
            "users.parquet",
            "join \"orders.parquet\" on .user_id | where .b.amount > 100 | top 5 by .b.amount",
            20,
        );
        assert!(
            s.contains("INNER JOIN read_parquet('orders.parquet') AS b ON a.user_id = b.user_id")
        );
        assert!(s.contains("WHERE b.amount > 100"));
        assert!(s.contains("ORDER BY b.amount DESC"));
        assert!(s.contains("LIMIT 5"));
    }

    #[test]
    fn join_right_side_hive_autodetect() {
        let s = cmp(
            "users.parquet",
            "join \"orders/dt=2026-*/*.parquet\" on .user_id",
            20,
        );
        // Right side is a hive-partitioned glob — should auto-enable hive_partitioning.
        assert!(
            s.contains("read_parquet('orders/dt=2026-*/*.parquet', hive_partitioning=true) AS b")
        );
    }

    #[test]
    fn join_left_outer() {
        let s = cmp(
            "users.parquet",
            "left_join \"orders.parquet\" on .user_id | select .a.email, .b.amount",
            20,
        );
        assert!(s.contains(
            "LEFT OUTER JOIN read_parquet('orders.parquet') AS b ON a.user_id = b.user_id"
        ));
        assert!(s.contains("SELECT a.email, b.amount"));
    }

    #[test]
    fn join_right_outer_space_form() {
        let s = cmp("a.parquet", "right join \"b.parquet\" on .id | count", 20);
        assert!(s.contains("RIGHT OUTER JOIN"));
    }

    #[test]
    fn join_full_outer() {
        let s = cmp("a.parquet", "full_join \"b.parquet\" on .id", 20);
        assert!(s.contains("FULL OUTER JOIN"));
    }

    #[test]
    fn join_outer_alias_for_full() {
        let s = cmp("a.parquet", "outer_join \"b.parquet\" on .id", 20);
        assert!(s.contains("FULL OUTER JOIN"));
    }

    #[test]
    fn join_inner_explicit() {
        let s = cmp("a.parquet", "inner_join \"b.parquet\" on .id", 20);
        assert!(s.contains("INNER JOIN"));
        // Sanity: no LEFT/RIGHT/FULL leaked in.
        assert!(!s.contains("OUTER JOIN"));
    }

    #[test]
    fn join_multi_key_via_and() {
        let s = cmp(
            "users.parquet",
            "left_join \"events.parquet\" on .a.id == .b.user_id and .a.dt == .b.dt | count",
            20,
        );
        assert!(
            s.contains("ON a.id = b.user_id and a.dt = b.dt"),
            "expected multi-key ON expression, got: {}",
            s
        );
    }

    #[test]
    fn to_csv_bare_emits_concat_ws() {
        let s = cmp("u.parquet", ".email, .country | to_csv", 20);
        // Outer SELECT references the inner subquery's final column names.
        assert!(
            s.contains("SELECT concat_ws(',', email::TEXT, country::TEXT) AS line FROM (SELECT email, country"),
            "got: {}",
            s
        );
    }

    #[test]
    fn to_csv_marks_raw_lines_in_compile_output() {
        let out = compile_plan("u.parquet", ".email, .country | to_csv", 20).unwrap();
        assert!(out.raw_lines);
    }

    #[test]
    fn to_json_uses_final_column_names() {
        // After a join, the inner subquery exposes columns named `email` and
        // `amount` (DuckDB strips the table alias). The outer struct keys
        // and values both reference those final names — NOT the raw `a.email` /
        // `b.amount` expressions.
        let s = cmp(
            "u.parquet",
            "join \"o.parquet\" on .id | select .a.email, .b.amount | to_json",
            20,
        );
        assert!(
            s.contains("to_json({\"email\": email, \"amount\": amount}) AS line FROM ("),
            "got: {}",
            s
        );
    }

    #[test]
    fn to_json_after_group_by_with_order() {
        // The bug that motivated the subquery wrap: `ORDER BY sum_revenue DESC`
        // can't run alongside a SELECT that's been folded to a single `line`
        // column. Wrapping fixes it because ORDER BY now lives in the inner
        // SELECT where the alias is in scope.
        let s = cmp(
            "s.parquet",
            "group_by .country | sum .revenue | sort by .sum_revenue desc | to_json",
            20,
        );
        assert!(
            s.contains(
                "to_json({\"country\": country, \"sum_revenue\": sum_revenue}) AS line FROM ("
            ),
            "got: {}",
            s
        );
        assert!(s.contains("ORDER BY sum_revenue DESC"));
    }

    #[test]
    fn to_json_without_explicit_columns_uses_struct_pack() {
        // Bare `to_json` (no projection) → output a JSON object of the entire
        // row by passing the table alias through DuckDB's struct-of-row coercion.
        let s = cmp("u.parquet", "to_json", 0);
        assert!(
            s.contains("SELECT to_json(__pq_inner) AS line FROM ("),
            "got: {}",
            s
        );
    }

    #[test]
    fn to_ndjson_and_to_jsonl_alias_to_json() {
        // v0.9.1 ergonomic aliases — the unix world reaches for "ndjson"
        // and "jsonl" by reflex when chaining `pq | jq | pq -i ndjson -`.
        // Both must compile to the exact same SQL as `to_json`.
        let baseline = cmp("u.parquet", ".email, .country | to_json", 20);
        let ndjson = cmp("u.parquet", ".email, .country | to_ndjson", 20);
        let jsonl = cmp("u.parquet", ".email, .country | to_jsonl", 20);
        assert_eq!(baseline, ndjson, "to_ndjson should alias to_json");
        assert_eq!(baseline, jsonl, "to_jsonl should alias to_json");
        let out = compile_plan("u.parquet", "to_ndjson", 0).unwrap();
        assert!(
            out.raw_lines,
            "to_ndjson must set raw_lines so the renderer skips header/quoting"
        );
    }

    #[test]
    fn join_explicit_different_column_names() {
        // users.id = orders.user_id — different col names on each side.
        assert_eq!(
            cmp(
                "users.parquet",
                "join \"orders.parquet\" on .a.id == .b.user_id | select .a.email, .b.amount",
                20
            ),
            "SELECT a.email, b.amount FROM read_parquet('users.parquet') AS a \
             INNER JOIN read_parquet('orders.parquet') AS b ON a.id = b.user_id"
        );
    }

    #[test]
    fn join_double_alias_columns_in_projection() {
        // `.a.col` should preserve the dot so both alias and column survive the rewrite.
        let s = cmp(
            "u.parquet",
            "join \"o.parquet\" on .id | .a.email, .b.amount",
            20,
        );
        assert!(s.contains("SELECT a.email, b.amount"));
    }

    #[test]
    fn min_max_aggregates_combined() {
        assert_eq!(
            cmp("f.parquet", "group_by .country | min .age | max .age", 20),
            "SELECT country, min(age) AS min_age, max(age) AS max_age \
             FROM read_parquet('f.parquet') GROUP BY country"
        );
    }

    // ── v0.10 nested-path tokenizer ─────────────────────────────────────────

    #[test]
    fn path_struct_dot_unchanged() {
        // Plain struct field access keeps DuckDB's auto-naming (`name`)
        // — backward-compat with v0.9.x users who already lean on this.
        assert_eq!(path_to_sql(".user.name").unwrap(), "user.name");
        assert!(alias_for_path(".user.name").is_none());
    }

    #[test]
    fn path_list_index_translates_jq_to_duckdb() {
        // jq's 0-indexed → DuckDB's 1-indexed for positive integers.
        assert_eq!(path_to_sql(".tags[0]").unwrap(), "tags[1]");
        assert_eq!(path_to_sql(".tags[5]").unwrap(), "tags[6]");
        assert_eq!(alias_for_path(".tags[0]").as_deref(), Some("tags_0"));
    }

    #[test]
    fn path_list_negative_index_passthrough() {
        // DuckDB natively supports `lst[-1]` for last-element access —
        // we don't translate, just rename the alias to a snake_case form
        // (`tags_neg1`) so it survives JSON serialization.
        assert_eq!(path_to_sql(".tags[-1]").unwrap(), "tags[-1]");
        assert_eq!(alias_for_path(".tags[-1]").as_deref(), Some("tags_neg1"));
    }

    #[test]
    fn path_unnest_wraps_subject() {
        assert_eq!(path_to_sql(".tags[]").unwrap(), "UNNEST(tags)");
        assert_eq!(alias_for_path(".tags[]").as_deref(), Some("tags"));
    }

    #[test]
    fn path_chained_unnest_renders_inline() {
        // v0.11: tokenizer no longer rejects mid-path `[]`. `path_to_sql`
        // emits the literal `UNNEST(...).suffix` form; the lifter in
        // `to_sql_core` is what hoists it into a derived FROM. Verifying
        // the path-level emission keeps that contract testable in
        // isolation.
        assert_eq!(
            path_to_sql(".events[].kind").unwrap(),
            "UNNEST(events).kind"
        );
        assert_eq!(
            path_to_sql(".events[].user.id").unwrap(),
            "UNNEST(events).user.id"
        );
        assert_eq!(
            path_to_sql(".matrix[][]").unwrap(),
            "UNNEST(UNNEST(matrix))"
        );
    }

    #[test]
    fn path_map_key_double_quoted() {
        // ["key"] and ['key'] both compile to element_at + [1].
        assert_eq!(
            path_to_sql(".metadata[\"plan\"]").unwrap(),
            "element_at(metadata, 'plan')[1]"
        );
        assert_eq!(
            path_to_sql(".metadata['plan']").unwrap(),
            "element_at(metadata, 'plan')[1]"
        );
        assert_eq!(
            alias_for_path(".metadata['plan']").as_deref(),
            Some("metadata_plan")
        );
    }

    #[test]
    fn path_map_key_with_apostrophe_escaped() {
        // SQL-injection-shaped key — escape via double-up.
        assert_eq!(
            path_to_sql(".m[\"o'brien\"]").unwrap(),
            "element_at(m, 'o''brien')[1]"
        );
    }

    #[test]
    fn path_chained_index_then_struct() {
        assert_eq!(path_to_sql(".events[0].kind").unwrap(), "events[1].kind");
        assert_eq!(
            alias_for_path(".events[0].kind").as_deref(),
            Some("events_0_kind")
        );
    }

    #[test]
    fn path_chained_struct_then_index() {
        // `.foo.bar[0]` — STRUCT field then list index.
        assert_eq!(path_to_sql(".foo.bar[2]").unwrap(), "foo.bar[3]");
    }

    #[test]
    fn projection_with_brackets_emits_aliases() {
        let s = cmp(
            "f.parquet",
            ".user_id, .tags[0], .events[0].amount, .metadata[\"plan\"]",
            0,
        );
        assert_eq!(
            s,
            "SELECT user_id, tags[1] AS tags_0, \
             events[1].amount AS events_0_amount, \
             element_at(metadata, 'plan')[1] AS metadata_plan \
             FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn projection_unnest_explosion() {
        let s = cmp("f.parquet", ".user_id, .tags[]", 0);
        assert_eq!(
            s,
            "SELECT user_id, UNNEST(tags) AS tags FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn path_function_len() {
        assert_eq!(
            rewrite_path_function("len(.tags)").as_deref(),
            Some("len(tags)")
        );
        assert_eq!(
            path_function_alias("len(.tags)").as_deref(),
            Some("len_tags")
        );
        let s = cmp("f.parquet", ".user_id, len(.tags)", 0);
        assert_eq!(
            s,
            "SELECT user_id, len(tags) AS len_tags FROM read_parquet('f.parquet')"
        );
    }

    #[test]
    fn path_function_keys_values() {
        assert_eq!(
            rewrite_path_function("keys(.metadata)").as_deref(),
            Some("map_keys(metadata)")
        );
        assert_eq!(
            rewrite_path_function("values(.metadata)").as_deref(),
            Some("map_values(metadata)")
        );
    }

    #[test]
    fn where_with_struct_path() {
        // STRUCT field comparisons inside WHERE — used to silently drop
        // the dot and emit `WHERE country = ...`, breaking against
        // schemas that have a top-level `country` AND a `.user.country`.
        let s = cmp("f.parquet", "where .user.country == \"US\"", 20);
        assert!(s.contains("WHERE user.country = 'US'"), "got: {}", s);
    }

    #[test]
    fn where_with_list_index() {
        // Numeric comparison on list element.
        let s = cmp("f.parquet", "where .events[0].amount > 100", 20);
        assert!(s.contains("WHERE events[1].amount > 100"), "got: {}", s);
    }

    #[test]
    fn where_with_map_key() {
        let s = cmp("f.parquet", "where .metadata[\"plan\"] == \"pro\"", 20);
        assert!(
            s.contains("WHERE element_at(metadata, 'plan')[1] = 'pro'"),
            "got: {}",
            s
        );
    }

    #[test]
    fn sort_by_nested_path() {
        let s = cmp("f.parquet", "sort by .events[0].amount desc", 20);
        assert!(s.contains("ORDER BY events[1].amount DESC"), "got: {}", s);
    }

    #[test]
    fn aggregate_on_nested_path() {
        let s = cmp("f.parquet", "sum .events[0].amount", 0);
        assert!(s.contains("sum(events[1].amount)"), "got: {}", s);
    }

    #[test]
    fn group_by_nested_path_strips_alias() {
        // Regression for the v0.10 GROUP BY bug spotted on real Clarivate
        // health data: parse_projection emits `expr AS alias` for bracket
        // paths, and we used to copy that whole string into GROUP BY,
        // producing a parse error. SELECT list keeps the alias; GROUP BY
        // takes the bare expression.
        let s = cmp(
            "f.parquet",
            "group_by .payer_zipped[0].type_coverage | count",
            0,
        );
        assert!(
            s.contains(
                "SELECT payer_zipped[1].type_coverage AS payer_zipped_0_type_coverage, \
                 count(*) AS count"
            ),
            "got: {}",
            s
        );
        assert!(
            s.contains("GROUP BY payer_zipped[1].type_coverage"),
            "GROUP BY should NOT include the AS alias suffix; got: {}",
            s
        );
        assert!(
            !s.contains("GROUP BY payer_zipped[1].type_coverage AS"),
            "GROUP BY must not have an AS clause; got: {}",
            s
        );
    }

    #[test]
    fn limit_head_accepts_unix_n_flag() {
        // Shell-pipe muscle memory reaches for `head -n 3` and `head -3`.
        // pq's DSL stage parser used to fail those with a literal
        // "got `-n 3`" error — actively unhelpful. Now both forms route
        // to the same `LIMIT N` SQL.
        for q in ["head -n 3", "head -3", "limit -n 3", "limit 3"] {
            let s = cmp("f.parquet", q, 0);
            assert!(
                s.contains("LIMIT 3"),
                "query `{}` should produce `LIMIT 3`; got: {}",
                q,
                s
            );
        }
    }

    #[test]
    fn projection_chained_unnest_lifts_to_subquery() {
        // v0.11: `.events[].kind` compiles by hoisting `UNNEST(events)`
        // into a derived FROM. The outer SELECT references `_pq_u0.kind`;
        // DuckDB never sees raw UNNEST in the outer context where it
        // would complain.
        let s = cmp("f.parquet", ".events[].kind", 0);
        assert!(
            s.contains("UNNEST(events) AS _pq_u0"),
            "missing inner unnest projection: {}",
            s
        );
        assert!(
            s.contains("_pq_u0.kind"),
            "outer SELECT should reference alias.field: {}",
            s
        );
        // Outer SELECT must NOT contain UNNEST anymore (it's all hoisted).
        let outer = s.split(" FROM ").next().unwrap();
        assert!(
            !outer.contains("UNNEST"),
            "outer SELECT should be UNNEST-free: {}",
            outer
        );
    }

    #[test]
    fn group_by_chained_unnest_compiles() {
        // The shape that triggered the original v0.11 ask:
        // `group_by .events[].kind | count` previously failed with
        // DuckDB's "UNNEST not supported here". Now it must produce a
        // well-formed SELECT whose GROUP BY references `_pq_u0.kind`.
        let s = cmp("f.parquet", "group_by .events[].kind | count", 0);
        assert!(s.contains("UNNEST(events) AS _pq_u0"));
        assert!(s.contains("_pq_u0.kind AS events_kind"));
        assert!(s.contains("GROUP BY _pq_u0.kind"));
        assert!(s.contains("count(*) AS count"));
    }

    #[test]
    fn bare_path_function_routes_to_projection() {
        // Regression for tutorial L2.3: `keys(.metadata)` as a stand-alone
        // stage used to fall through `parse_stage`'s last-resort branch and
        // emit `WHERE map_keys(metadata)` — DuckDB then complained the
        // function didn't exist, because the WHERE path doesn't run path
        // rewrites. Now bare path-function calls reach `parse_projection`,
        // which routes through `rewrite_path_function` and aliases the
        // result.
        let s = cmp("f.parquet", "keys(.metadata)", 0);
        assert!(
            s.contains("SELECT map_keys(metadata)"),
            "keys(.metadata) should project map_keys(): {}",
            s
        );
        assert!(
            !s.contains("WHERE"),
            "keys() alone shouldn't synthesize a WHERE: {}",
            s
        );
        // `len(.events)` and `values(.metadata)` exercise the same arm.
        assert!(cmp("f.parquet", "len(.events)", 0).contains("SELECT len(events)"));
        assert!(cmp("f.parquet", "values(.metadata)", 0).contains("SELECT map_values(metadata)"));
    }

    #[test]
    fn shared_unnest_source_dedupes() {
        // `.events[].kind, .events[].amount` should NOT produce two
        // independent unnests of `events` (which would yield N*N rows).
        // The lifter dedupes on the inner expression so both columns
        // share `_pq_u0`.
        let s = cmp("f.parquet", ".events[].kind, .events[].amount", 0);
        assert_eq!(
            s.matches("UNNEST(events) AS _pq_u0").count(),
            1,
            "should emit exactly one UNNEST(events) in inner: {}",
            s
        );
        assert!(
            !s.contains("_pq_u1"),
            "should not allocate a 2nd alias: {}",
            s
        );
    }

    #[test]
    fn where_with_chained_unnest_compiles() {
        // WHERE emits force-lift, so even chained UNNEST in a filter
        // routes through the inner subquery.
        let s = cmp("f.parquet", "where .events[].kind == \"click\"", 0);
        assert!(s.contains("UNNEST(events) AS _pq_u0"));
        assert!(s.contains("WHERE _pq_u0.kind = 'click'"));
    }

    #[test]
    fn strip_as_alias_helper() {
        assert_eq!(strip_as_alias("foo"), "foo");
        assert_eq!(strip_as_alias("foo AS bar"), "foo");
        assert_eq!(strip_as_alias("foo as bar"), "foo");
        assert_eq!(
            strip_as_alias("payer_zipped[1].type_coverage AS payer_zipped_0_type_coverage"),
            "payer_zipped[1].type_coverage"
        );
    }
}
