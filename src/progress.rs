//! v0.13 — stderr spinner + elapsed timer for long-running CLI queries.
//!
//! Background — without a progress indicator, `pq … '…' big.parquet`
//! against a 30 GB file just freezes the terminal for tens of
//! seconds. Users can't tell whether pq is hung, the network
//! stalled, or DuckDB is grinding away normally. A faint stderr
//! spinner ("⠋ 1.2s elapsed — Ctrl-C to cancel") removes the
//! ambiguity for the cost of one extra thread.
//!
//! Design choices:
//!   * stderr-only so stdout stays a clean stream of rows.
//!     Pipelines like `pq … | jq …` are unaffected.
//!   * Skips entirely when stderr isn't a TTY — CI / log files
//!     shouldn't be polluted with carriage-return spam.
//!   * Skips when --no-progress / PQ_NO_PROGRESS=1 is set.
//!   * Holds drawing for the first 300 ms so short queries (the
//!     common case) never flash a spinner. The threshold is the
//!     usual UX rule of thumb for "users perceive instant".
//!   * Drops on Drop — the spinner thread polls a stop flag every
//!     ~80 ms, clears its line, and exits. No lifecycle gymnastics.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Eight-dot Braille spinner, same set used by `pip`, `cargo`, and
/// pretty much every modern CLI. Rotates monospace-clean on every
/// terminal we care about.
const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Hold-off before the first spinner draw. Queries that finish
/// faster than this never render a spinner — good for `pq count` /
/// `pq head` / quick filters where any flash would be noise.
const HOLD_OFF: Duration = Duration::from_millis(300);

/// Re-draw cadence. 80 ms ≈ 12.5 fps — fast enough to look animated,
/// slow enough that we don't burn an entire CPU on a tight loop.
const TICK: Duration = Duration::from_millis(80);

/// RAII handle. Drop signals the worker to clear its line and exit.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner unless the caller asked to suppress it or
    /// stderr isn't a TTY. `disabled=true` (typically the
    /// `--no-progress` flag) wins over the env var; the env var is
    /// the script-side opt-out.
    pub fn maybe_start(disabled: bool) -> Option<Self> {
        if disabled {
            return None;
        }
        if std::env::var_os("PQ_NO_PROGRESS").is_some() {
            return None;
        }
        if !std::io::stderr().is_terminal() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let handle = thread::spawn(move || run(stop_w));
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }
}

fn run(stop: Arc<AtomicBool>) {
    let started = Instant::now();
    let mut tick = 0usize;
    let mut drew_anything = false;
    while !stop.load(Ordering::Relaxed) {
        if started.elapsed() >= HOLD_OFF {
            let secs = started.elapsed().as_secs_f32();
            let frame = FRAMES[tick % FRAMES.len()];
            // \r returns to col 0; the trailing space pads short
            // lines so leftover digits from a previous draw
            // (e.g. "10.5s" → "9.6s ") don't ghost.
            let _ = write!(
                std::io::stderr(),
                "\r{} {:>5.1}s elapsed — Ctrl-C to cancel ",
                frame,
                secs
            );
            let _ = std::io::stderr().flush();
            tick += 1;
            drew_anything = true;
        }
        thread::sleep(TICK);
    }
    // Erase whatever we drew so the next stderr write starts fresh.
    // Fast path: if we never drew (query was shorter than HOLD_OFF),
    // skip the carriage-return write entirely.
    if drew_anything {
        let _ = write!(std::io::stderr(), "\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--no-progress` always wins, regardless of TTY status. This is
    /// the script-side opt-out: a CI job that wants a clean stderr
    /// can pass `--no-progress` even when running under a fake TTY.
    #[test]
    fn maybe_start_returns_none_when_disabled() {
        let s = Spinner::maybe_start(true);
        assert!(s.is_none(), "disabled flag should suppress spawn");
    }

    /// Test runners (`cargo test`) don't have a TTY for stderr — and
    /// neither do CI / pipe-captured invocations. The spinner must
    /// stay silent in all of those.
    #[test]
    fn maybe_start_returns_none_when_stderr_is_not_tty() {
        let s = Spinner::maybe_start(false);
        assert!(
            s.is_none(),
            "spinner shouldn't start when stderr isn't a tty"
        );
    }

    /// `PQ_NO_PROGRESS=1` is the env-var equivalent of `--no-progress`
    /// — useful for shell aliases and CI configs that can't easily
    /// thread an extra flag through.
    #[test]
    fn maybe_start_respects_env_opt_out() {
        // SAFETY: we restore the var before returning so other tests
        // running in the same process aren't affected.
        let prev = std::env::var_os("PQ_NO_PROGRESS");
        unsafe {
            std::env::set_var("PQ_NO_PROGRESS", "1");
        }
        let s = Spinner::maybe_start(false);
        // Restore before asserting so a panic doesn't leak the var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("PQ_NO_PROGRESS", v),
                None => std::env::remove_var("PQ_NO_PROGRESS"),
            }
        }
        assert!(s.is_none(), "PQ_NO_PROGRESS=1 should suppress spawn");
    }
}
