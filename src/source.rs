//! Input source resolution + stdin auto-spooling (v0.9).
//!
//! Two responsibilities:
//!
//! 1. **InputFormat** — pq accepts parquet (default), ndjson (jsonl), and csv.
//!    The format determines which DuckDB `read_*` table function we emit
//!    inside `parser::source_clause`.
//!
//! 2. **StdinSpool** — Parquet's footer-trailer layout means the reader has
//!    to seek to EOF before it can decode anything. When the user pipes a
//!    parquet file in (`cat f.parquet | pq -` or `aws s3 cp s3://x/y - | pq -`)
//!    the fd handed to us is an anonymous pipe, which is not seekable —
//!    DuckDB rejects it with ESPIPE. We work around that by detecting the
//!    non-seekable case and draining stdin into a NamedTempFile, then
//!    redirecting the source path to that file. RAII via `tempfile`
//!    cleans the file up when the spool guard drops at the end of main().
//!
//!    NDJSON and CSV are line-oriented and DuckDB streams them through the
//!    pipe directly — no spool needed.

use anyhow::{Context, Result};
use std::io;
use std::path::PathBuf;

/// On-disk encoding of the input. Drives which DuckDB `read_*` function we
/// use, and whether stdin needs to be spooled to a temp file before the
/// engine can touch it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InputFormat {
    Parquet,
    /// Newline-delimited JSON (a.k.a. JSONL). DuckDB streams this without
    /// needing seek, so a `cat f.ndjson | pq -i ndjson -` pipeline Just Works.
    Ndjson,
    Csv,
}

impl InputFormat {
    /// CLI `--input` value → format. `auto` returns None and lets the caller
    /// fall back to extension-sniffing or the default.
    pub fn from_flag(name: &str) -> Result<Option<Self>> {
        match name {
            "auto" => Ok(None),
            "parquet" => Ok(Some(Self::Parquet)),
            "ndjson" | "jsonl" | "json" => Ok(Some(Self::Ndjson)),
            "csv" => Ok(Some(Self::Csv)),
            other => Err(anyhow::anyhow!(
                "unknown --input format '{}': expected auto|parquet|ndjson|csv",
                other
            )),
        }
    }

    /// Best-effort sniff from a path's extension. Used when `--input auto`
    /// (the default). Stdin paths have no extension, so callers should
    /// special-case `-` before reaching here.
    pub fn from_extension(path: &str) -> Self {
        let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "ndjson" | "jsonl" => Self::Ndjson,
            "json" => Self::Ndjson, // we treat .json as ndjson for the chain case
            "csv" | "tsv" => Self::Csv,
            _ => Self::Parquet,
        }
    }
}

// Note on stdin spooling: every format `pq` supports needs random access
// to the input. Parquet has the obvious footer-trailer reason. NDJSON and
// CSV look streamable but DuckDB's `read_json` / `read_csv_auto` actually
// run a schema-inference pass over the input first and a row-decoding
// pass second — that's two reads, which a pipe can't satisfy (you can
// see this empirically as a 0-row result on `cat f.ndjson | pq -i ndjson -`
// when we tried to skip the spool). The fix: spool every non-seekable
// stdin to a tempfile, regardless of format. See StdinSpool::resolve.

/// Holder for the in-flight stdin tempfile. Keeping the `NamedTempFile`
/// alive in main() prevents it from being deleted before DuckDB finishes
/// reading; dropping it at the end of main() cleans up.
///
/// `path` is the resolved file string we hand to `parser::source_clause`
/// in place of `-`. It's empty iff the input wasn't stdin (no spool needed).
pub struct StdinSpool {
    /// Holds the tempfile alive. Dropping deletes the underlying file.
    /// `_holder == None` means we're using `/dev/stdin` directly (the input
    /// is seekable already, e.g. shell `<` redirect) or the input wasn't stdin.
    _holder: Option<tempfile::NamedTempFile>,
    /// Resolved path string to feed into `read_parquet('...')` etc.
    /// Equal to the original `file` for non-stdin inputs.
    pub resolved: String,
}

impl StdinSpool {
    /// Decide what to do with the user's `file` argument:
    ///
    /// * non-stdin path → no-op, returns the path unchanged
    /// * `-` + seekable stdin (shell `<` redirect on most platforms)
    ///   → use `/dev/stdin` directly
    /// * `-` + non-seekable stdin (anonymous pipe)
    ///   → drain to a tempfile, return its path
    ///
    /// `fmt` controls whether we even bother with the spool path. For
    /// formats that DuckDB can stream (ndjson/csv) we always pass
    /// `/dev/stdin`, regardless of seekability.
    pub fn resolve(file: &str, fmt: InputFormat) -> Result<Self> {
        if file != "-" {
            return Ok(Self {
                _holder: None,
                resolved: file.to_string(),
            });
        }
        // If the fd is already seekable (e.g. `pq - < f.parquet`) we can
        // use /dev/stdin directly — DuckDB happily seeks within a regular
        // file fd. The spool path is only for non-seekable cases (anonymous
        // pipes from `cat | pq` / `aws s3 cp - | pq` / chain `pq | pq`).
        // For parquet only — line-oriented formats need the spool even on
        // seekable stdins because read_json/read_csv_auto's schema-
        // inference pass rewinds after sampling, and even a regular file
        // /dev/stdin gets confused there. Cost is one extra fwrite of the
        // input.
        if stdin_is_seekable() && fmt == InputFormat::Parquet {
            return Ok(Self {
                _holder: None,
                resolved: "/dev/stdin".to_string(),
            });
        }
        // Suffix matters for some DuckDB readers' format dispatch (e.g.
        // read_json's auto-detection looks at the extension) — match it
        // to the requested format so the spooled file feels native.
        let suffix = match fmt {
            InputFormat::Parquet => ".parquet",
            InputFormat::Ndjson => ".ndjson",
            InputFormat::Csv => ".csv",
        };
        let mut tf = tempfile::Builder::new()
            .prefix("pq-stdin-")
            .suffix(suffix)
            .tempfile()
            .context("failed to create stdin spool tempfile")?;
        // Stream stdin → tempfile. io::copy uses an 8 KB buffer; for parquet
        // files even at multi-GB this finishes in a single sequential write
        // pass, which is roughly the same cost as the read DuckDB would do
        // anyway. The tempfile lives in $TMPDIR (RAM-backed on macOS, /tmp
        // on linux) so this is usually backed by tmpfs.
        let mut stdin = io::stdin().lock();
        io::copy(&mut stdin, tf.as_file_mut())
            .context("failed to drain stdin into spool tempfile")?;
        let path: PathBuf = tf.path().to_path_buf();
        Ok(Self {
            _holder: Some(tf),
            resolved: path.to_string_lossy().into_owned(),
        })
    }
}

/// Probe whether fd 0 (stdin) supports seek. Pipes / sockets / fifos return
/// `ESPIPE` from `lseek(2)`; regular files / block devices succeed.
///
/// We use libc's lseek directly rather than
/// `std::fs::File::from_raw_fd(0) + seek` because the latter would bind
/// the `File` to the fd's lifetime — dropping the borrowed `File` would
/// close stdin, which we very much need to keep open for the spool path.
/// `lseek` is non-destructive.
fn stdin_is_seekable() -> bool {
    // SAFETY: fd 0 is always valid for the duration of the process (even
    // when /dev/stdin isn't readable, the syscall itself just returns EBADF
    // or ESPIPE — never crashes). SEEK_CUR with offset 0 is the canonical
    // "is this fd seekable?" probe; it doesn't move the cursor.
    unsafe { libc::lseek(0, 0, libc::SEEK_CUR) >= 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_format_flag_parsing() {
        assert_eq!(InputFormat::from_flag("auto").unwrap(), None);
        assert_eq!(
            InputFormat::from_flag("parquet").unwrap(),
            Some(InputFormat::Parquet)
        );
        assert_eq!(
            InputFormat::from_flag("ndjson").unwrap(),
            Some(InputFormat::Ndjson)
        );
        assert_eq!(
            InputFormat::from_flag("jsonl").unwrap(),
            Some(InputFormat::Ndjson)
        );
        assert_eq!(
            InputFormat::from_flag("csv").unwrap(),
            Some(InputFormat::Csv)
        );
        assert!(InputFormat::from_flag("xml").is_err());
    }

    #[test]
    fn input_format_extension_sniff() {
        assert_eq!(
            InputFormat::from_extension("data.parquet"),
            InputFormat::Parquet
        );
        assert_eq!(
            InputFormat::from_extension("data.ndjson"),
            InputFormat::Ndjson
        );
        assert_eq!(
            InputFormat::from_extension("data.jsonl"),
            InputFormat::Ndjson
        );
        assert_eq!(
            InputFormat::from_extension("data.json"),
            InputFormat::Ndjson
        );
        assert_eq!(InputFormat::from_extension("data.csv"), InputFormat::Csv);
        assert_eq!(InputFormat::from_extension("data.tsv"), InputFormat::Csv);
        // Unknown / no extension → parquet (the historical default).
        assert_eq!(InputFormat::from_extension("data"), InputFormat::Parquet);
        assert_eq!(
            InputFormat::from_extension("data.xyz"),
            InputFormat::Parquet
        );
    }

    #[test]
    fn spool_passes_through_non_stdin() {
        // A regular file path should be returned unchanged, no tempfile.
        let s = StdinSpool::resolve("foo.parquet", InputFormat::Parquet).unwrap();
        assert_eq!(s.resolved, "foo.parquet");
        assert!(s._holder.is_none());
    }
}
