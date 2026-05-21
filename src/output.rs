// Output rendering: pretty TUI table for terminals, JSON / NDJSON / CSV for pipes.

use anyhow::{Context, Result};
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, ContentArrangement, Table};
use duckdb::types::Value;
use duckdb::Connection;
use serde_json::{Map, Value as JsonValue};
use std::io::{self, IsTerminal, Write};

#[derive(Copy, Clone, Debug)]
pub enum OutputFormat {
    Table,
    Json,
    Ndjson,
    Csv,
}

impl OutputFormat {
    /// "auto" picks Table for a TTY, NDJSON for a pipe.
    pub fn resolve(name: &str) -> OutputFormat {
        match name {
            "table" => OutputFormat::Table,
            "json" => OutputFormat::Json,
            "ndjson" => OutputFormat::Ndjson,
            "csv" => OutputFormat::Csv,
            "auto" | _ => {
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

    let column_names: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names())
        .unwrap_or_default();
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
    }
}

fn print_table(cols: &[String], rows: &[Vec<Value>]) -> Result<()> {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(cols.iter().map(|c| Cell::new(c)));

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
        Value::Date32(d) => JsonValue::String(format!("date(days={})", d)),
        Value::Time64(_, t) => JsonValue::String(format!("time({})", t)),
        Value::Timestamp(_, t) => JsonValue::String(format!("timestamp({})", t)),
        other => JsonValue::String(format!("{:?}", other)),
    }
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
