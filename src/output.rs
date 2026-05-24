// Output rendering: pretty TUI table for terminals, JSON / NDJSON / CSV / Parquet for pipes.

use anyhow::{Context, Result};
use comfy_table::{Cell, ContentArrangement, Table};

/// Custom preset: pure single-line box drawing.
///
/// Comfy-table's stock UTF8_FULL/UTF8_FULL_CONDENSED presets use `┆` (dashed
/// vertical) between cells and `╞═╪╡` (heavy/double-line) for the header
/// separator. Several macOS fonts don't render those glyphs at the expected
/// width, which makes rows look staggered or misaligned in the terminal.
/// We mirror DuckDB's CLI: only `│ ─ ┌ ┐ ├ ┤ ┬ ┴ ┼` — a charset every
/// modern terminal/font handles uniformly.
///
/// Field order (from comfy_table::style::presets): left, right, bottom, top,
/// header_left, header_line, header_intersect, header_right, vertical,
/// horizontal, intersect, intersect_left, intersect_right, top_intersect,
/// bottom_intersect, top_left, top_right, bottom_left, bottom_right.
///
/// Spaces at indices 9–12 mean "don't draw the row separator inside the
/// body" — same behaviour as comfy_table's `*_CONDENSED` presets, so we
/// only get a single line between header and data and clean rows below.
const PQ_TABLE_PRESET: &str = "││──├─┼┤│    ┬┴┌┐└┘";
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
        .load_preset(PQ_TABLE_PRESET)
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
        Value::Time64(unit, t) => JsonValue::String(time_to_iso(*unit, *t)),
        Value::Timestamp(unit, t) => JsonValue::String(timestamp_to_iso(*unit, *t)),
        // v0.10: nested types render as proper JSON arrays / objects so the
        // chain idiom (`pq f.parquet '.events' | jq ... | pq -i ndjson -`)
        // works without raw-SQL escape hatches. Before this change we fell
        // through to `format!("{:?}", other)` which produced Rust Debug
        // output ("List([Text(\"a\")])") that downstream tools can't parse.
        Value::List(items) | Value::Array(items) => {
            JsonValue::Array(items.iter().map(value_to_json).collect())
        }
        Value::Struct(fields) => {
            // OrderedMap preserves declaration order — keep that in JSON.
            let mut m = serde_json::Map::new();
            for (k, v) in fields.iter() {
                m.insert(k.clone(), value_to_json(v));
            }
            JsonValue::Object(m)
        }
        Value::Map(entries) => {
            // JSON object keys are always strings; coerce non-string keys
            // through the standard display path. Rare — most parquet MAP
            // columns we see use VARCHAR keys (session_id, plan, etc.).
            let mut m = serde_json::Map::new();
            for (k, v) in entries.iter() {
                let key_string = match k {
                    Value::Text(s) => s.clone(),
                    other => value_to_display(other),
                };
                m.insert(key_string, value_to_json(v));
            }
            JsonValue::Object(m)
        }
        Value::Enum(s) => JsonValue::String(s.clone()),
        // Union wraps an inner Value of whichever branch matched at row time.
        Value::Union(inner) => value_to_json(inner),
        other => JsonValue::String(format!("{:?}", other)),
    }
}

/// Split a TIMESTAMP/TIME tick count into (whole_seconds, sub_second_nanos)
/// based on its `TimeUnit`. `div_euclid`/`rem_euclid` keep negative inputs
/// well-behaved (the alternative truncates toward zero, which gives a
/// nonsensical "negative nanos" for pre-epoch timestamps).
fn split_unit(unit: duckdb::types::TimeUnit, value: i64) -> (i64, i64) {
    use duckdb::types::TimeUnit::*;
    match unit {
        Second => (value, 0),
        Millisecond => (value.div_euclid(1_000), value.rem_euclid(1_000) * 1_000_000),
        Microsecond => (
            value.div_euclid(1_000_000),
            value.rem_euclid(1_000_000) * 1_000,
        ),
        Nanosecond => (
            value.div_euclid(1_000_000_000),
            value.rem_euclid(1_000_000_000),
        ),
    }
}

/// Format sub-second nanos as `.fff…` with trailing zeros stripped, or
/// empty string when nanos == 0. Pulls the conditional out of the call
/// sites so the timestamp formatter stays readable.
fn format_fractional(nanos: i64) -> String {
    if nanos == 0 {
        String::new()
    } else {
        let s = format!("{nanos:09}");
        let trimmed = s.trim_end_matches('0');
        format!(".{trimmed}")
    }
}

/// Convert a Parquet/DuckDB `TIMESTAMP` (TimeUnit + tick count since
/// 1970-01-01 UTC) to ISO 8601 `YYYY-MM-DDTHH:MM:SS[.fff]`. No timezone
/// suffix — DuckDB's plain TIMESTAMP is wall-clock (naive); rendering a
/// Z would be a lie. Pre-epoch values render correctly thanks to
/// `div_euclid`/`rem_euclid` plus the proleptic-Gregorian `date32_to_iso`.
fn timestamp_to_iso(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let (secs, nanos) = split_unit(unit, value);
    let days = secs.div_euclid(86_400) as i32;
    let secs_of_day = secs.rem_euclid(86_400);
    let h = secs_of_day / 3_600;
    let m = (secs_of_day % 3_600) / 60;
    let s = secs_of_day % 60;
    let date = date32_to_iso(days);
    let frac = format_fractional(nanos);
    format!("{date}T{h:02}:{m:02}:{s:02}{frac}")
}

/// Convert a `TIME` value (tick count *within* a day, no date component) to
/// `HH:MM:SS[.fff]`. We don't take a modulo because legitimate TIME values
/// always live in `[0, 86_400_000_000_000)` for nanos — DuckDB enforces this.
fn time_to_iso(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let (secs, nanos) = split_unit(unit, value);
    let h = secs / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    let frac = format_fractional(nanos);
    format!("{h:02}:{m:02}:{s:02}{frac}")
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

/// Single-cell display string for a DuckDB Value. Shared with the TUI
/// (which used to ship its own copy missing TIMESTAMP / nested-type
/// handling) so date/struct/list rendering stays consistent across all
/// output paths.
pub(crate) fn value_to_display(v: &Value) -> String {
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
        Value::Time64(unit, t) => time_to_iso(*unit, *t),
        Value::Timestamp(unit, t) => timestamp_to_iso(*unit, *t),
        // v0.10: nested types render as compact JSON in tables/CSV so they
        // sit cleanly in a single cell. We reuse value_to_json to get the
        // structural representation, then serde_json's compact serializer.
        Value::List(_)
        | Value::Array(_)
        | Value::Struct(_)
        | Value::Map(_)
        | Value::Enum(_)
        | Value::Union(_) => {
            serde_json::to_string(&value_to_json(v)).unwrap_or_else(|_| format!("{:?}", v))
        }
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
    fn timestamp_micros_renders_iso() {
        // 2026-01-01 17:00:00 UTC = 1_767_286_800 seconds since epoch.
        // duckdb's default TIMESTAMP is microsecond precision.
        let micros = 1_767_286_800_i64 * 1_000_000;
        assert_eq!(
            timestamp_to_iso(duckdb::types::TimeUnit::Microsecond, micros),
            "2026-01-01T17:00:00"
        );
    }

    #[test]
    fn timestamp_with_subsecond_strips_trailing_zeros() {
        // 1970-01-01T00:00:00.123 — millisecond precision, fractional part
        // should render as `.123` (not `.123000` or `.123000000`).
        assert_eq!(
            timestamp_to_iso(duckdb::types::TimeUnit::Millisecond, 123),
            "1970-01-01T00:00:00.123"
        );
        // nanosecond precision keeps full precision when it's there.
        assert_eq!(
            timestamp_to_iso(duckdb::types::TimeUnit::Nanosecond, 123_456_789),
            "1970-01-01T00:00:00.123456789"
        );
    }

    #[test]
    fn timestamp_pre_epoch_handles_borrow_correctly() {
        // 1969-12-31T23:59:59 — one second before epoch. Naive `secs / 86400`
        // truncates-toward-zero gives day=0 with secs_of_day=-1, which would
        // render as 1970-01-01T-1 nonsense. The Euclidean split yields
        // (-1 day, 86399 secs) → 1969-12-31T23:59:59, the right answer.
        assert_eq!(
            timestamp_to_iso(duckdb::types::TimeUnit::Second, -1),
            "1969-12-31T23:59:59"
        );
    }

    #[test]
    fn time_renders_iso_no_date() {
        // 14:30:00 in microseconds = 14*3600 + 30*60 = 52_200 seconds
        let micros = 52_200_i64 * 1_000_000;
        assert_eq!(
            time_to_iso(duckdb::types::TimeUnit::Microsecond, micros),
            "14:30:00"
        );
    }

    #[test]
    fn date32_known_values() {
        assert_eq!(date32_to_iso(20_592), "2026-05-19");
        assert_eq!(date32_to_iso(19_358), "2023-01-01");
        assert_eq!(date32_to_iso(-1), "1969-12-31");
    }

    // ── v0.10 nested-type renderer ─────────────────────────────────────────

    use duckdb::types::OrderedMap;
    use serde_json::json;

    #[test]
    fn list_renders_as_json_array() {
        let v = Value::List(vec![
            Value::Text("a".into()),
            Value::Text("b".into()),
            Value::Null,
        ]);
        assert_eq!(value_to_json(&v), json!(["a", "b", null]));
        assert_eq!(value_to_display(&v), r#"["a","b",null]"#);
    }

    #[test]
    fn struct_preserves_field_order() {
        // OrderedMap preserves declaration order — verify the JSON we
        // emit honours that contract (matters for snapshot consumers
        // and for users who pipe to jq with positional logic).
        let fields = OrderedMap::from(vec![
            ("name".to_string(), Value::Text("alice".into())),
            ("country".to_string(), Value::Text("US".into())),
            ("age".to_string(), Value::Int(30)),
        ]);
        let v = Value::Struct(fields);
        let s = serde_json::to_string(&value_to_json(&v)).unwrap();
        assert_eq!(s, r#"{"name":"alice","country":"US","age":30}"#);
    }

    #[test]
    fn map_renders_as_json_object_with_string_keys() {
        let entries = OrderedMap::from(vec![
            (Value::Text("plan".into()), Value::Text("pro".into())),
            (Value::Text("seat".into()), Value::Text("3".into())),
        ]);
        let v = Value::Map(entries);
        assert_eq!(value_to_json(&v), json!({"plan":"pro","seat":"3"}));
    }

    #[test]
    fn list_of_struct_round_trips() {
        // The shape that hits LIST<STRUCT> in real parquet files
        // (event arrays, line items, etc.). Was the worst pre-v0.10
        // bug — used to dump as `List([Struct(OrderedMap([...]))])`.
        let row1 = OrderedMap::from(vec![
            ("kind".to_string(), Value::Text("click".into())),
            ("amount".to_string(), Value::Double(1.0)),
        ]);
        let row2 = OrderedMap::from(vec![
            ("kind".to_string(), Value::Text("buy".into())),
            ("amount".to_string(), Value::Double(9.0)),
        ]);
        let v = Value::List(vec![Value::Struct(row1), Value::Struct(row2)]);
        assert_eq!(
            value_to_json(&v),
            json!([
                {"kind":"click","amount":1.0},
                {"kind":"buy","amount":9.0},
            ])
        );
    }

    #[test]
    fn empty_collections_serialize_correctly() {
        assert_eq!(value_to_json(&Value::List(vec![])), json!([]));
        assert_eq!(
            value_to_json(&Value::Struct(OrderedMap::from(vec![]))),
            json!({})
        );
        assert_eq!(
            value_to_json(&Value::Map(OrderedMap::from(vec![]))),
            json!({})
        );
    }

    #[test]
    fn nested_value_csv_quotes_when_needed() {
        let v = Value::List(vec![Value::Text("a,b".into()), Value::Text("c".into())]);
        // Has a comma in its compact JSON ([... , ...]) so should be quoted.
        let cell = value_to_csv(&v);
        assert!(
            cell.starts_with('"') && cell.ends_with('"'),
            "got: {}",
            cell
        );
    }
}
