// Token-level lineage for the pq DSL.
//
// Why this exists separately from `parser.rs`:
//
// The TUI wants to highlight columns the user is *currently typing*, which
// almost always means the query is syntactically incomplete (mid-word, dangling
// pipe, missing operand). The full parser bails on the first error, so we'd
// lose the cursor's lineage on every other keystroke.
//
// This module instead does a forgiving lexical scan that:
//   - finds every `.ident` token (column references) and remembers their byte
//     spans, so the TUI can map cursor offset → source column;
//   - finds every aggregator → column pairing (`sum .revenue`, `count`, etc.)
//     and computes the alias DuckDB will eventually assign (`sum_revenue`,
//     `count`), so the Data-panel column header can be reverse-mapped to its
//     source field for cross-panel highlighting;
//   - never errors out — it just reports what it found.
//
// Quote handling matches `split_pipe_stages` in parser.rs (single + double
// quote awareness) so column references inside string literals don't get
// mistaken for real refs.

use crate::parser::alias_safe;

/// Span of a `.ident` token within the source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    /// Bare column name (without the leading `.`).
    pub name: String,
    /// Byte offset of the leading `.`.
    pub start: usize,
    /// Byte offset just past the last identifier byte (one past end, like Rust ranges).
    pub end: usize,
}

/// A derived column produced by an aggregator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedColumn {
    /// The alias DuckDB will assign in the result row, e.g. "sum_revenue", "count".
    pub alias: String,
    /// Aggregator keyword: "sum", "avg", "min", "max", "count", "count_distinct".
    pub agg: String,
    /// Source column. None for bare `count` (which aggregates `*`).
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Lineage {
    pub column_refs: Vec<ColumnRef>,
    pub derived: Vec<DerivedColumn>,
}

impl Lineage {
    /// Find the column reference whose span covers `byte_offset`. We use a
    /// half-open match (start ≤ off ≤ end) so a cursor parked just past the
    /// last identifier byte still counts as "on" the token — the textarea
    /// places the cursor *after* the char you just typed, which is exactly
    /// where users expect highlighting to fire.
    pub fn column_at(&self, byte_offset: usize) -> Option<&ColumnRef> {
        self.column_refs
            .iter()
            .find(|c| byte_offset >= c.start && byte_offset <= c.end)
    }

    /// Reverse-map a derived alias (e.g. "sum_revenue") back to the source
    /// column it aggregates over. Used by the Data panel: when the user's
    /// column-cursor lands on `sum_revenue`, the Columns panel highlights
    /// `revenue`.
    pub fn source_of(&self, alias: &str) -> Option<&str> {
        self.derived
            .iter()
            .find(|d| d.alias == alias)
            .and_then(|d| d.source.as_deref())
    }
}

/// Lex `query` into a `Lineage`. Never errors — partial / malformed queries
/// just yield whatever lineage we could observe.
pub fn extract(query: &str) -> Lineage {
    let bytes = query.as_bytes();
    let mut out = Lineage::default();
    let mut i = 0;
    let mut last_keyword: Option<(String, usize)> = None; // (kw, end_idx)
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let c = bytes[i] as char;

        // Quote tracking — copy-paste of split_pipe_stages's logic so we
        // ignore content inside string literals.
        if c == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_single || in_double {
            i += 1;
            continue;
        }

        // .ident → ColumnRef. We allow the very common "qualified" form
        // `.a.col` (table-qualified column from a join) by greedily eating
        // dotted segments, but only emit one ColumnRef for the *whole* span
        // — using the *last* segment as the source-column name. That's what
        // matches Columns-panel rows: users see `country` listed there, not
        // `a.country`.
        if c == '.' && i + 1 < bytes.len() && is_ident_start(bytes[i + 1]) {
            let start = i;
            i += 1;
            // Eat one or more dot-segments.
            let mut last_seg_start = i;
            loop {
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                if i + 1 < bytes.len() && bytes[i] == b'.' && is_ident_start(bytes[i + 1]) {
                    i += 1;
                    last_seg_start = i;
                } else {
                    break;
                }
            }
            let name = std::str::from_utf8(&bytes[last_seg_start..i])
                .unwrap_or("")
                .to_string();
            let cref = ColumnRef {
                name: name.clone(),
                start,
                end: i,
            };
            out.column_refs.push(cref);

            // If the immediately-preceding keyword was an aggregator, emit
            // a DerivedColumn linking this column ref to the alias DuckDB
            // would produce. We use only the *last* dotted segment as the
            // source — same as Aggregate::alias() in parser.rs.
            if let Some((kw, kw_end)) = last_keyword.take() {
                if is_agg_keyword(&kw) && between_is_blank(&query[kw_end..start]) {
                    out.derived.push(DerivedColumn {
                        alias: agg_alias(&kw, Some(&name)),
                        agg: kw,
                        source: Some(name),
                    });
                }
            }
            continue;
        }

        // Bare keyword scan — only at word boundaries. Used to detect
        // aggregator → column pairings AND the bare `count` form.
        if is_ident_start(c as u8) {
            // Skip if we're in the middle of a longer identifier (e.g. `account`
            // contains `count`). Walk back: previous byte must be word-boundary.
            let prev_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
            if prev_ok {
                let kw_start = i;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let kw = &query[kw_start..i];

                // Bare `count` — emits a DerivedColumn with source=None. We
                // recognize it only when *not* immediately followed by `(` or
                // `_distinct`, to avoid double-counting `count(*)` or
                // `count_distinct .col` (which is handled below as a normal
                // aggregator pairing).
                if kw.eq_ignore_ascii_case("count") {
                    let next_nonblank = next_nonblank_byte(bytes, i);
                    let is_call = next_nonblank == Some(b'(');
                    let is_distinct_chain =
                        kw.eq_ignore_ascii_case("count") && peek_underscore_distinct(bytes, i);
                    if !is_call && !is_distinct_chain {
                        out.derived.push(DerivedColumn {
                            alias: "count".to_string(),
                            agg: "count".to_string(),
                            source: None,
                        });
                        // Don't carry `count` as a pending aggregator — bare
                        // `count` doesn't take a column.
                        last_keyword = None;
                        continue;
                    }
                }

                if is_agg_keyword(kw) {
                    last_keyword = Some((kw.to_string(), i));
                } else {
                    // Any other identifier breaks the pending aggregator
                    // (so `sum where .x > 1` doesn't pair `sum` with `.x`).
                    last_keyword = None;
                }
                continue;
            }
        }

        i += 1;
    }

    out
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True for any aggregator keyword we know how to derive an alias for.
fn is_agg_keyword(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "sum" | "avg" | "min" | "max" | "count" | "count_distinct"
    )
}

/// Compute the DuckDB output alias for an aggregator + source column. Mirrors
/// `Aggregate::alias()` in parser.rs — keep the two in sync.
fn agg_alias(agg: &str, source: Option<&str>) -> String {
    let agg_l = agg.to_ascii_lowercase();
    match (agg_l.as_str(), source) {
        ("count", _) => "count".into(),
        ("count_distinct", Some(c)) => format!("count_distinct_{}", alias_safe(c)),
        ("sum", Some(c)) => format!("sum_{}", alias_safe(c)),
        ("avg", Some(c)) => format!("avg_{}", alias_safe(c)),
        ("min", Some(c)) => format!("min_{}", alias_safe(c)),
        ("max", Some(c)) => format!("max_{}", alias_safe(c)),
        // Defensive — should never hit with our parser, but cheap to guard.
        (other, Some(c)) => format!("{other}_{}", alias_safe(c)),
        (other, None) => other.into(),
    }
}

/// True iff every byte in `s` is whitespace. Used to verify the gap between
/// an aggregator keyword and its `.col` argument is just spaces.
fn between_is_blank(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_whitespace())
}

/// Peek past whitespace from `start`, return the next non-blank byte (or None
/// at end-of-input). Cheap helper to disambiguate `count` vs `count(*)`.
fn next_nonblank_byte(bytes: &[u8], start: usize) -> Option<u8> {
    let mut j = start;
    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
        j += 1;
    }
    bytes.get(j).copied()
}

/// True if the chars right after `start` spell `_distinct` (case-insensitive
/// boundary check). We've just consumed `count`; if the next thing is
/// `_distinct` glued on, this was actually `count_distinct` and we should
/// not treat the bare-`count` branch as fired.
fn peek_underscore_distinct(bytes: &[u8], start: usize) -> bool {
    let needle = b"_distinct";
    if start + needle.len() > bytes.len() {
        return false;
    }
    bytes[start..start + needle.len()]
        .iter()
        .zip(needle.iter())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(refs: &[ColumnRef]) -> Vec<&str> {
        refs.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn extracts_simple_projection() {
        let l = extract(".email, .country, .age");
        assert_eq!(names(&l.column_refs), vec!["email", "country", "age"]);
        assert!(l.derived.is_empty());
    }

    #[test]
    fn ignores_columns_inside_string_literals() {
        let l = extract(".email where .country == \"US.tld\"");
        // Only `.email` and `.country` should count — the `.tld` is inside
        // a quoted string and must not produce a ColumnRef.
        assert_eq!(names(&l.column_refs), vec!["email", "country"]);
    }

    #[test]
    fn detects_qualified_dotted_columns() {
        // `.a.id == .b.user_id` — only the last segment is the column name.
        let l = extract("on .a.id == .b.user_id");
        let n = names(&l.column_refs);
        assert_eq!(n, vec!["id", "user_id"]);
    }

    #[test]
    fn aggregator_pairing_sum() {
        let l = extract("group_by .country | sum .revenue");
        assert_eq!(
            l.derived,
            vec![DerivedColumn {
                alias: "sum_revenue".into(),
                agg: "sum".into(),
                source: Some("revenue".into()),
            }]
        );
    }

    #[test]
    fn aggregator_pairing_count_distinct() {
        let l = extract("count_distinct .email");
        assert_eq!(
            l.derived,
            vec![DerivedColumn {
                alias: "count_distinct_email".into(),
                agg: "count_distinct".into(),
                source: Some("email".into()),
            }]
        );
    }

    #[test]
    fn bare_count_emits_derived_with_no_source() {
        let l = extract("group_by .country | count");
        assert!(
            l.derived
                .iter()
                .any(|d| d.alias == "count" && d.source.is_none()),
            "expected bare count among {:?}",
            l.derived
        );
    }

    #[test]
    fn bare_count_not_emitted_for_count_paren() {
        // `count(*)` is raw SQL passthrough — not our DSL form. The lineage
        // scanner sees the keyword but should NOT emit a bare-count derived
        // entry because the `(` follows.
        let l = extract("count(*)");
        assert!(
            l.derived.is_empty(),
            "no DSL-style derived from count(*): {:?}",
            l.derived
        );
    }

    #[test]
    fn bare_count_not_confused_with_count_distinct() {
        // We see `count` first; the next chars are `_distinct .email`. The
        // bare-count branch must defer to count_distinct here.
        let l = extract("count_distinct .email");
        let bare_counts = l
            .derived
            .iter()
            .filter(|d| d.alias == "count" && d.source.is_none())
            .count();
        assert_eq!(
            bare_counts, 0,
            "should not emit bare count: {:?}",
            l.derived
        );
    }

    #[test]
    fn column_at_finds_token_under_cursor() {
        let q = ".email, .country";
        let l = extract(q);
        // Cursor on `email` (between `.` and the comma)
        let off = q.find("email").unwrap() + 2;
        let r = l.column_at(off).expect("expected hit");
        assert_eq!(r.name, "email");
        // Cursor on `country`
        let off = q.find("country").unwrap() + 1;
        let r = l.column_at(off).expect("expected hit");
        assert_eq!(r.name, "country");
        // Cursor in the gap → no hit
        let off = q.find(", ").unwrap();
        // The `,` itself sits one byte past the end of `email` — that should
        // still resolve to `email` because column_at uses inclusive end.
        let r = l.column_at(off);
        assert!(r.is_some_and(|c| c.name == "email"));
        // Mid-gap, definitely outside any token
        let off = off + 1; // on the space
        assert!(l.column_at(off).is_none());
    }

    #[test]
    fn source_of_round_trip() {
        let l = extract("group_by .country | sum .revenue | top 3 by sum_revenue");
        assert_eq!(l.source_of("sum_revenue"), Some("revenue"));
        // Unknown alias → None.
        assert_eq!(l.source_of("nonexistent"), None);
    }

    #[test]
    fn aggregator_pairing_breaks_on_intervening_token() {
        // `sum where .x > 1` — the `where` keyword between `sum` and `.x`
        // should clear the pending aggregator, so we do NOT mis-pair.
        let l = extract("sum where .x > 1");
        assert!(
            l.derived.is_empty(),
            "no spurious sum_x pairing: {:?}",
            l.derived
        );
    }

    #[test]
    fn nested_struct_path_uses_last_segment() {
        let l = extract(".user.address.city");
        // We only emit one ColumnRef (the whole dotted span), with the last
        // segment as the canonical column name — matches what shows up in
        // DESCRIBE output for the unnested column.
        assert_eq!(names(&l.column_refs), vec!["city"]);
    }
}
