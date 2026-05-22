// Output rendering: pretty TUI table for terminals, JSON / NDJSON / CSV / Parquet for pipes.

use anyhow::{Context, Result};
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, ContentArrangement, Table};
use duckdb::types::Value;
use duckdb::Connection;
use serde_json::{Map, Value as JsonValue};
use std::io::{self, IsTerminal, Write};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
    Ndjson,
    Csv,
    Parquet,
    /// One row per line, single TEXT column printed as-is. Used when a query
    /// ends in `to_csv` / `to_json` — the renderer doesn't add quoting,
    /// headers, or JSON wrapping; what DuckDB returns is what stdout gets.
    RawLines,
}

impl OutputFormat {
    /// "auto" picks Table for a TTY, NDJSON for a pipe.
    pub fn resolve(name: &str) -> OutputFormat {
        match name {
            "table" => OutputFormat::Table,
            "json" => OutputFormat::Json,
            "ndjson" => OutputFormat::Ndjson,
            "csv" => OutputFormat::Csv,
            "parquet" => OutputFormat::Parquet,
            _ => {
                // "auto" and any unknown name fall here.
                if io::stdout().is_terminal() {
                    OutputFormat::Table
                } else {
                    OutputFormat::Ndjson
                }
            }
        }
    }
}

pub fn run_and_print(conn: &Connection, sql: &str, fmt: OutputFormat) -> Result<()> {
    if fmt == OutputFormat::Parquet {
        return run_parquet(conn, sql);
    }

    let mut stmt = conn.prepare(sql).with_context(|| {
        format!(
            "DuckDB rejected the generated SQL.\n  SQL: {}\n  hint: re-run with --explain to inspect.",
            sql
        )
    })?;

    // IMPORTANT: schema (and therefore column_names) is only populated after the
    // statement is executed — call query() FIRST, then read column metadata.
    let mut rows = stmt.query([]).with_context(|| {
        format!(
            "DuckDB failed to execute the generated SQL.\n  SQL: {}\n  hint: re-run with --explain to inspect.",
            sql
        )
    })?;

    let column_names: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
    let ncols = column_names.len();

    let mut collected: Vec<Vec<Value>> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut r = Vec::with_capacity(ncols);
        for i in 0..ncols {
            r.push(row.get::<usize, Value>(i).unwrap_or(Value::Null));
        }
        collected.push(r);
    }

    match fmt {
        OutputFormat::Table => print_table(&column_names, &collected),
        OutputFormat::Json => print_json(&column_names, &collected),
        OutputFormat::Ndjson => print_ndjson(&column_names, &collected),
        OutputFormat::Csv => print_csv(&column_names, &collected),
        OutputFormat::RawLines => print_raw_lines(&collected),
        OutputFormat::Parquet => unreachable!("Parquet handled before row iteration"),
    }
}

/// Print one row per line, taking the first column's value verbatim.
/// The query is expected to have collapsed every selected col into a single
/// TEXT column (via `to_csv` / `to_json`), so we only ever read column 0.
fn print_raw_lines(rows: &[Vec<Value>]) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for r in rows {
        // value_to_display already turns NULL → "∅"; for raw-lines we want
        // empty string instead so awk/jq pipelines don't see junk.
        let line = match r.first() {
            Some(Value::Null) | None => String::new(),
            Some(v) => value_to_display(v),
        };
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Write the query result as a parquet file to stdout.
///
/// We can't pipe DuckDB's COPY TO directly to stdout because parquet's footer
/// (with column-chunk byte offsets) is written last and requires a seekable
/// file descriptor. So: COPY to a temp file, then `io::copy` it to stdout, then
/// best-effort delete the temp file. The cost (one extra disk write) is
/// negligible vs. the alternative of buffering the whole result in memory.
fn run_parquet(conn: &Connection, sql: &str) -> Result<()> {
    let pid = std::process::id();
    // Use a nanosecond timestamp to avoid collisions if multiple pq's race.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("pq-{pid}-{nanos}.parquet"));

    // RAII cleanup: delete temp file even if we panic / error mid-stream.
    struct TmpFile<'a>(&'a std::path::Path);
    impl Drop for TmpFile<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _guard = TmpFile(&tmp);

    let escaped = tmp.to_string_lossy().replace('\'', "''");
    let copy_sql = format!("COPY ({sql}) TO '{escaped}' (FORMAT PARQUET)");
    conn.execute_batch(&copy_sql).with_context(|| {
        format!(
            "DuckDB failed to write parquet output.\n  SQL: {}\n  hint: re-run with --explain to inspect.",
            copy_sql
        )
    })?;

    let mut f = std::fs::File::open(&tmp)
        .with_context(|| format!("failed to open temp parquet at {}", tmp.display()))?;
    let stdout = io::stdout();
    let mut h = stdout.lock();
    io::copy(&mut f, &mut h).context("failed to stream parquet to stdout")?;
    Ok(())
}

fn print_table(cols: &[String], rows: &[Vec<Value>]) -> Result<()> {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(cols.iter().map(Cell::new));

    for r in rows {
        table.add_row(r.iter().map(|v| Cell::new(value_to_display(v))));
    }
    println!("{}", table);

    let stderr = io::stderr();
    let mut h = stderr.lock();
    let _ = writeln!(h, "({} rows)", rows.len());
    Ok(())
}

fn print_json(cols: &[String], rows: &[Vec<Value>]) -> Result<()> {
    let arr: Vec<JsonValue> = rows.iter().map(|r| row_to_json(cols, r)).collect();
    println!("{}", serde_json::to_string_pretty(&JsonValue::Array(arr))?);
    Ok(())
}

fn print_ndjson(cols: &[String], rows: &[Vec<Value>]) -> Result<()> {
    let stdout = io::stdout();
    let mut h = stdout.lock();
    for r in rows {
        let line = serde_json::to_string(&row_to_json(cols, r))?;
        writeln!(h, "{}", line)?;
    }
    Ok(())
}

fn print_csv(cols: &[String], rows: &[Vec<Value>]) -> Result<()> {
    let stdout = io::stdout();
    let mut h = stdout.lock();
    writeln!(h, "{}", cols.join(","))?;
    for r in rows {
        let cells: Vec<String> = r.iter().map(value_to_csv).collect();
        writeln!(h, "{}", cells.join(","))?;
    }
    Ok(())
}

fn row_to_json(cols: &[String], row: &[Value]) -> JsonValue {
    let mut m = Map::new();
    for (i, c) in cols.iter().enumerate() {
        m.insert(c.clone(), value_to_json(row.get(i).unwrap_or(&Value::Null)));
    }
    JsonValue::Object(m)
}

fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::TinyInt(i) => JsonValue::Number((*i as i64).into()),
        Value::SmallInt(i) => JsonValue::Number((*i as i64).into()),
        Value::Int(i) => JsonValue::Number((*i as i64).into()),
        Value::BigInt(i) => JsonValue::Number((*i).into()),
        Value::HugeInt(i) => JsonValue::String(i.to_string()),
        Value::UTinyInt(i) => JsonValue::Number((*i as u64).into()),
        Value::USmallInt(i) => JsonValue::Number((*i as u64).into()),
        Value::UInt(i) => JsonValue::Number((*i as u64).into()),
        Value::UBigInt(i) => JsonValue::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f as f64)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::Double(f) => serde_json::Number::from_f64(*f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::Decimal(d) => {
            // Emit decimals as JSON strings to preserve precision.
            JsonValue::String(d.to_string())
        }
        Value::Text(s) => JsonValue::String(s.clone()),
        Value::Blob(b) => JsonValue::String(format!("<blob {} bytes>", b.len())),
        Value::Date32(d) => JsonValue::String(date32_to_iso(*d)),
        Value::Time64(_, t) => JsonValue::String(format!("time({})", t)),
        Value::Timestamp(_, t) => JsonValue::String(format!("timestamp({})", t)),
        other => JsonValue::String(format!("{:?}", other)),
    }
}

/// Convert days since 1970-01-01 (parquet/arrow Date32 representation) to
/// "YYYY-MM-DD". Implements Howard Hinnant's `civil_from_days` algorithm —
/// proleptic Gregorian, no external date library needed.
fn date32_to_iso(days: i32) -> String {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146_096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn value_to_display(v: &Value) -> String {
    match v {
        Value::Null => "∅".to_string(),
        Value::Text(s) => s.clone(),
        Value::Boolean(b) => b.to_string(),
        Value::Float(f) => format!("{}", f),
        Value::Double(f) => format!("{}", f),
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
        Value::Date32(d) => date32_to_iso(*d),
        other => format!("{:?}", other),
    }
}

fn value_to_csv(v: &Value) -> String {
    let s = value_to_display(v);
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date32_unix_epoch_zero() {
        assert_eq!(date32_to_iso(0), "1970-01-01");
    }

    #[test]
    fn date32_known_values() {
        assert_eq!(date32_to_iso(20_592), "2026-05-19");
        assert_eq!(date32_to_iso(19_358), "2023-01-01");
        assert_eq!(date32_to_iso(-1), "1969-12-31");
    }
}
