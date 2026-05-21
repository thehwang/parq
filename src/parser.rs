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

// ─── Source ──────────────────────────────────────────────────────────────────

/// Wrap a path/URI into a DuckDB FROM-clause-friendly source expression.
pub fn source_clause(file: &str) -> String {
    let f = file.trim();
    if f == "-" {
        return "read_parquet('/dev/stdin')".to_string();
    }
    let escaped = f.replace('\'', "''");
    format!("read_parquet('{}')", escaped)
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
            Aggregate::CountDistinct(c) => format!("count(DISTINCT {c}) AS count_distinct_{c}"),
            Aggregate::Sum(c) => format!("sum({c}) AS sum_{c}"),
            Aggregate::Avg(c) => format!("avg({c}) AS avg_{c}"),
            Aggregate::Min(c) => format!("min({c}) AS min_{c}"),
            Aggregate::Max(c) => format!("max({c}) AS max_{c}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct QueryPlan {
    pub source: String,           // "read_parquet('...')"
    pub projections: Vec<String>, // explicit SELECT columns (non-aggregate)
    pub aggregates: Vec<Aggregate>,
    pub filters: Vec<String>, // WHERE
    pub group_by: Vec<String>,
    pub havings: Vec<String>, // HAVING (filters that arrive after a grouping verb)
    pub order_by: Vec<(String, bool)>, // (col_or_expr, ascending)
    pub limit: Option<usize>,
    pub distinct: bool,
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

    pub fn to_sql(&self) -> String {
        let mut sql = String::with_capacity(128);
        sql.push_str("SELECT ");
        if self.distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&self.select_list());
        sql.push_str(" FROM ");
        sql.push_str(&self.source);

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
}

// ─── compile() ───────────────────────────────────────────────────────────────

pub fn compile(file: &str, query: &str, default_limit: usize) -> Result<String> {
    let src = source_clause(file);
    let q = query.trim();

    // Empty query → head with default limit
    if q.is_empty() {
        let limit = if default_limit == 0 {
            String::new()
        } else {
            format!(" LIMIT {default_limit}")
        };
        return Ok(format!("SELECT * FROM {src}{limit}"));
    }

    // Raw SQL passthrough (escape hatch). FILE token is substituted with our source clause.
    let lower_first = q.chars().take(8).collect::<String>().to_ascii_lowercase();
    if lower_first.starts_with("select ") || lower_first.starts_with("with ") {
        return Ok(q.replace("FILE", &src));
    }

    let mut plan = QueryPlan::new(src);
    for stage in split_pipe_stages(q) {
        parse_stage(stage, &mut plan)?;
    }
    Ok(plan.to_sql())
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
        compile(file, q, n).unwrap()
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
    fn glob_path_passthrough() {
        let s = cmp("data/dt=2026-*/*.parquet", "count", 20);
        assert!(s.contains("read_parquet('data/dt=2026-*/*.parquet')"));
        assert!(s.contains("count(*)"));
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
