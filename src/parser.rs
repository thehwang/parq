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
    col.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

    /// What ends up after SELECT, BEFORE any line_format wrapping.
    fn select_list(&self) -> String {
        if self.has_grouping() {
            let mut parts: Vec<String> = self.group_by.clone();
            for a in &self.aggregates {
                parts.push(a.sql());
            }
            if parts.is_empty() {
                "*".into()
            } else {
                parts.join(", ")
            }
        } else if !self.projections.is_empty() {
            self.projections.join(", ")
        } else {
            "*".into()
        }
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
    fn to_sql_core(&self) -> String {
        let mut sql = String::with_capacity(128);
        sql.push_str("SELECT ");
        if self.distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&self.select_list());
        sql.push_str(" FROM ");
        sql.push_str(&self.source);

        // Join (INNER/LEFT/RIGHT/FULL OUTER), single equi-condition, aliases left=a / right=b.
        if let Some(j) = &self.join {
            sql.push_str(" AS a ");
            sql.push_str(j.kind.sql());
            sql.push(' ');
            sql.push_str(&j.right_source);
            sql.push_str(" AS b ON ");
            sql.push_str(&j.on_expr);
        }

        if !self.filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&self.filters.join(" AND "));
        }
        if !self.group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            sql.push_str(&self.group_by.join(", "));
        }
        if !self.havings.is_empty() {
            sql.push_str(" HAVING ");
            sql.push_str(&self.havings.join(" AND "));
        }
        if !self.order_by.is_empty() {
            sql.push_str(" ORDER BY ");
            let parts: Vec<String> = self
                .order_by
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
            let col = strip_dot(rest);
            if col.is_empty() {
                return Err(anyhow!(
                    "`{}` requires a column (e.g. `{} .revenue`)",
                    kw,
                    kw
                ));
            }
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

    // limit N / head N
    if let Some(rest) = strip_keyword(s, "limit").or_else(|| strip_keyword(s, "head")) {
        let n: usize = rest
            .trim()
            .parse()
            .map_err(|_| anyhow!("`limit/head` needs an integer, got `{}`", rest.trim()))?;
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
    if s.starts_with('.') {
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

/// Split "col" / ".col" / "col asc" / ".col desc" — returns (col_name, ascending)
fn parse_orderby_clause(s: &str, default_asc: bool) -> (String, bool) {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(stripped) = lower.strip_suffix(" desc") {
        (strip_dot(&s[..stripped.len()]), false)
    } else if let Some(stripped) = lower.strip_suffix(" asc") {
        (strip_dot(&s[..stripped.len()]), true)
    } else {
        (strip_dot(s), default_asc)
    }
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

    let cols: Vec<String> = s
        .split(',')
        .map(|c| {
            let c = c.trim();
            if let Some(stripped) = c.strip_prefix('.') {
                stripped.trim().to_string()
            } else {
                c.to_string()
            }
        })
        .filter(|c| !c.is_empty())
        .collect();

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
                i += 1;
                continue;
            }
        }

        s.push(c);
        i += 1;
    }
    s
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
}
