// DSL → DuckDB SQL compiler.
//
// Grammar (v0):
//
//   query        := projection
//                 | filter_expr
//                 | projection 'where' filter_expr
//                 | raw_sql                       -- starts with SELECT/WITH
//                 | <empty>                       -- => head LIMIT n
//
//   projection   := ('select')? '.' ident ( ',' '.' ident )*
//                 | '.' ident ( '.' ident )*       -- nested path, single column
//
//   filter_expr  := <DuckDB SQL fragment>          -- with two sugar rewrites:
//                     '==' → '='
//                     bare '.col' → 'col'
//
// Source resolution accepts:
//   - local path (foo.parquet, ./data/*.parquet)
//   - glob ('data/dt=2026-*/*.parquet')
//   - gs://, s3://, az://, http(s)://
//   - "-" for stdin (read from /dev/stdin)

use anyhow::{anyhow, Result};

/// Wrap a path/URI into a DuckDB FROM clause.
/// We always go through `read_parquet(...)` so cloud paths and globs Just Work.
pub fn source_clause(file: &str) -> String {
    let f = file.trim();
    if f == "-" {
        // DuckDB doesn't read parquet from stdin natively; tell users to use a fifo.
        // We still emit something parseable so the error from DuckDB is informative.
        return "read_parquet('/dev/stdin')".to_string();
    }
    let escaped = f.replace('\'', "''");
    format!("read_parquet('{}')", escaped)
}

/// Compile (file, query, default_limit) into a single DuckDB SQL string.
pub fn compile(file: &str, query: &str, default_limit: usize) -> Result<String> {
    let src = source_clause(file);
    let q = query.trim();

    if q.is_empty() {
        let limit = if default_limit == 0 {
            "".to_string()
        } else {
            format!(" LIMIT {}", default_limit)
        };
        return Ok(format!(
            "SELECT * FROM {src}{limit}",
            src = src,
            limit = limit
        ));
    }

    // Raw SQL pass-through (escape hatch for power users)
    let lower = q.to_ascii_lowercase();
    if lower.starts_with("select ") || lower.starts_with("with ") {
        // Replace the first occurrence of a placeholder if present — for v0 we just
        // execute the user's SQL verbatim and assume they reference the file directly,
        // OR they use the literal token "FILE" which we substitute.
        let s = q.replace("FILE", &src);
        return Ok(s);
    }

    // count is a common shorthand
    if lower == "count" || lower == "count(*)" {
        return Ok(format!("SELECT count(*) AS rows FROM {src}", src = src));
    }

    // Split on " where " (case-insensitive, naive — doesn't handle quoted strings
    // containing the word "where", but that's a v0 acceptable shortcut).
    let (proj_part, filter_part) = match split_where(q) {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };

    // Decide projection
    let projection = match (
        proj_part,
        q.starts_with('.') || q.to_ascii_lowercase().starts_with("select "),
    ) {
        (Some(p), _) => parse_projection(p)?,
        (None, true) => parse_projection(q)?,
        (None, false) => "*".to_string(),
    };

    // Decide filter
    let filter_clause = match (filter_part, projection.as_str()) {
        (Some(f), _) => Some(rewrite_filter(f)),
        (None, "*") => Some(rewrite_filter(q)),
        _ => None,
    };

    let mut sql = format!(
        "SELECT {projection} FROM {src}",
        projection = projection,
        src = src
    );
    if let Some(f) = filter_clause {
        sql.push_str(&format!(" WHERE {}", f));
    }
    Ok(sql)
}

/// Case-insensitive search for the keyword " where " outside of quoted strings.
/// Returns (before_where, after_where) trimmed.
fn split_where(s: &str) -> Option<(&str, &str)> {
    let needle = " where ";
    let lower = s.to_ascii_lowercase();
    let mut in_single = false;
    let mut in_double = false;
    let bytes = lower.as_bytes();
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
            // strip leading dot if present (jq-style)
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

/// Rewrite a filter expression to DuckDB SQL.
///
///   "..."   → '...'   (jq-style strings → SQL string literals; literal ' inside
///                       gets escaped as '')
///   '...'   → '...'   (already SQL-correct, emitted verbatim)
///   ==      → =
///   !=      → <>
///   .col    → col     (only outside any quoted string, and only when the dot
///                       isn't part of an existing identifier like `tbl.col`)
fn rewrite_filter(input: &str) -> String {
    let mut s = String::with_capacity(input.len() + 4);
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut in_single = false; // inside a SQL-style '...'
    let mut in_double = false; // inside a jq-style "..." being rewritten to '...'

    while i < chars.len() {
        let c = chars[i];

        // SQL-style single-quoted string: emit verbatim, including '' escapes.
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

        // jq-style double-quoted string → rewrite delimiters to '...'
        if c == '"' {
            in_double = !in_double;
            s.push('\'');
            i += 1;
            continue;
        }
        if in_double {
            // We're emitting '...'; any literal ' inside the source must be doubled.
            if c == '\'' {
                s.push('\'');
                s.push('\'');
            } else {
                s.push(c);
            }
            i += 1;
            continue;
        }

        // Outside any string literal.

        // ==  →  =
        if c == '=' && chars.get(i + 1) == Some(&'=') {
            s.push('=');
            i += 2;
            continue;
        }

        // !=  →  <>
        if c == '!' && chars.get(i + 1) == Some(&'=') {
            s.push('<');
            s.push('>');
            i += 2;
            continue;
        }

        // .ident → ident (jq-style column reference, but only when this dot
        // isn't part of an existing identifier like `tbl.col` or numeric `3.14`).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(file: &str, q: &str, n: usize) -> String {
        compile(file, q, n).unwrap()
    }

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
        // user already wrote SQL-correct quoting — don't touch it
        assert_eq!(
            cmp("f.parquet", "country = 'US'", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE country = 'US'"
        );
    }

    #[test]
    fn apostrophe_inside_jq_string_gets_escaped() {
        // O'Brien inside "..." must become 'O''Brien' in SQL
        assert_eq!(
            cmp("f.parquet", "name == \"O'Brien\"", 20),
            "SELECT * FROM read_parquet('f.parquet') WHERE name = 'O''Brien'"
        );
    }

    #[test]
    fn qualified_column_dot_preserved() {
        // tbl.col should NOT lose the dot
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
        assert!(cmp("f.parquet", "count", 20).contains("count(*)"));
    }
}
