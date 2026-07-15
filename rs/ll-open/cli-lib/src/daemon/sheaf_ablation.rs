//! Sheaf ablation instrumentation — bead `ley-line-open-2775a3`.
//!
//! Optional log emitter that records, on every watcher-driven
//! `daemon.sheaf.invalidate` emit, the pair `(sheaf_count, naive_count)`
//! for the ablation study documented in
//! `docs/research/sheaf-ablation-study.md`. The load-bearing v0.7.0
//! marketing claim is that the sheaf-driven fine-grained diff touches
//! ≤ 30% of regions on average vs the naive "invalidate every known
//! region on file change" baseline; this module produces the numbers
//! that either back or falsify that claim.
//!
//! # Contract
//!
//! - **Opt-in.** Enabled iff `LEYLINE_SHEAF_ABLATION_LOG=<path>` is set
//!   in the daemon's environment at emit time. Unset → zero cost, no I/O.
//! - **Measurement-only.** Never mutates the emit payload; the naive
//!   baseline is computed for the log alone and NEVER sent on the wire.
//! - **Best effort.** File-open / write failures log at `warn` and drop
//!   the event; the production emit is untouched.
//! - **One JSON line per event.** Enables trivial `wc -l` + `jq` analysis.
//!
//! # Wire format
//!
//! Each line is a JSON object with these fields:
//!
//! ```json
//! {"ts_ms": 1720000000000,
//!  "changed_files": ["a.rs", "b.rs"],
//!  "sheaf_count": 3,
//!  "naive_count": 42,
//!  "sheaf_region_ids": [7, 12, 19],
//!  "naive_region_ids": [1, 2, 3, ..., 42],
//!  "scope": "changed-only"}
//! ```
//!
//! `scope` mirrors the on-wire emit's scope — `"changed-only"` when the
//! fine-grained diff was computed (label map installed), `"all-known"`
//! when the emit fell back to the coarse baseline. Analysis discards
//! `all-known` events because the ratio there is definitionally 1.0
//! (sheaf == naive) and provides no signal about the fine-grained diff's
//! precision.

use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// Env var that toggles ablation logging. Empty / unset → no-op.
pub const ABLATION_LOG_ENV: &str = "LEYLINE_SHEAF_ABLATION_LOG";

/// Serialized handle to the ablation log file. Behind a `Mutex` so
/// concurrent emits (there is at most one in-process today, but the
/// contract shouldn't rely on that) serialize writes rather than
/// interleaving partial lines.
static LOG_HANDLE: Mutex<Option<(PathBuf, std::fs::File)>> = Mutex::new(None);

/// Record one ablation event to the log, if the log is enabled.
///
/// - `sheaf_region_ids`: what the production emit sends on the wire.
/// - `naive_region_ids`: what a coarse `all-known` emit WOULD send —
///   every currently-known region ID in the CellComplex.
/// - `changed_files`: files that drove this emit.
/// - `scope`: `"changed-only"` (fine-grained diff computed) or
///   `"all-known"` (coarse fallback, sheaf == naive by construction).
///
/// Cheap early-out when the env var is unset. Doesn't panic on any
/// failure — just logs at `warn` and returns.
pub fn log_event(
    sheaf_region_ids: &[u32],
    naive_region_ids: &[u32],
    changed_files: &[String],
    scope: &str,
) {
    // Env-var check FIRST — this is the zero-cost early-out that makes
    // the instrumentation safe to leave enabled in production builds.
    // `std::env::var` is one syscall; the atomic-load-then-branch on the
    // static Mutex would be cheaper but requires an extra state variable
    // and only saves nanoseconds we're not paying to a hot loop today.
    let Ok(path_str) = std::env::var(ABLATION_LOG_ENV) else {
        return;
    };
    if path_str.is_empty() {
        return;
    }
    let path = PathBuf::from(path_str);

    let ts_ms = super::now_ms();
    let payload = serde_json::json!({
        "ts_ms": ts_ms,
        "changed_files": changed_files,
        "sheaf_count": sheaf_region_ids.len(),
        "naive_count": naive_region_ids.len(),
        "sheaf_region_ids": sheaf_region_ids,
        "naive_region_ids": naive_region_ids,
        "scope": scope,
    });

    let line = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("sheaf ablation: JSON encode failed: {e:#}");
            return;
        }
    };

    let mut slot = LOG_HANDLE.lock();
    // Reopen the file if the path changed (env var edited between
    // emits) — supports the harness pattern that sets a fresh log path
    // per workload run without a daemon restart.
    let need_reopen = match slot.as_ref() {
        Some((existing, _)) => existing != &path,
        None => true,
    };
    if need_reopen {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                *slot = Some((path.clone(), f));
            }
            Err(e) => {
                log::warn!("sheaf ablation: open {} failed: {e:#}", path.display(),);
                return;
            }
        }
    }
    if let Some((_, file)) = slot.as_mut()
        && let Err(e) = writeln!(file, "{line}")
    {
        log::warn!("sheaf ablation: write failed: {e:#}");
    }
}

/// Test-only helper: forget the cached file handle so a subsequent
/// [`log_event`] call reopens the path from the env var. Needed for
/// tests that recycle log paths within one process — otherwise the
/// stale `File` from a prior run keeps writing to a (possibly deleted)
/// inode.
#[doc(hidden)]
pub fn reset_handle_for_tests() {
    let mut slot = LOG_HANDLE.lock();
    *slot = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Env var unset → no I/O, no panic. The load-bearing zero-cost
    /// invariant: production daemons that never opt in pay nothing.
    ///
    /// Serialized against every other test that mutates
    /// `LEYLINE_SHEAF_ABLATION_LOG` via
    /// `#[serial("env_LEYLINE_SHEAF_ABLATION_LOG")]` — bead
    /// `ley-line-open-d71cf6`. Without this, the sister test's
    /// `set_var → set_handle → log_event × 2 → read_file` sequence
    /// races with this test's `remove_var` and appended 1 line instead
    /// of 2 under Ubuntu CI (observed 2026-07-13 on PR #184 run
    /// 29271365230).
    #[test]
    #[serial(env_LEYLINE_SHEAF_ABLATION_LOG)]
    fn log_event_is_noop_when_env_var_unset() {
        // SAFETY: env access is process-global. `#[serial(...)]`
        // above serializes this test against every other test in the
        // crate that mutates the same env var; the SAFETY invariant
        // is now compile-checked-by-label instead of a hand-waved
        // comment.
        unsafe {
            std::env::remove_var(ABLATION_LOG_ENV);
        }
        log_event(
            &[1, 2, 3],
            &[1, 2, 3, 4, 5],
            &["a.rs".into()],
            "changed-only",
        );
        // Nothing to assert other than "didn't panic and didn't create a file".
    }

    /// Env var pointing at a writable path → one JSON line per call.
    ///
    /// Serialized against `log_event_is_noop_when_env_var_unset` via
    /// `#[serial("env_LEYLINE_SHEAF_ABLATION_LOG")]` — bead
    /// `ley-line-open-d71cf6`. See the sister test for the
    /// observed-flake context.
    #[test]
    #[serial(env_LEYLINE_SHEAF_ABLATION_LOG)]
    fn log_event_appends_json_line_when_env_var_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("ablation.jsonl");
        // SAFETY: env access is process-global. `#[serial(...)]`
        // above serializes this test against every other test in the
        // crate that mutates the same env var.
        unsafe {
            std::env::set_var(ABLATION_LOG_ENV, &log_path);
        }
        reset_handle_for_tests();

        log_event(
            &[7, 12, 19],
            &[1, 2, 3, 7, 12, 19, 42],
            &["src/foo.rs".into()],
            "changed-only",
        );
        log_event(&[], &[1, 2, 3, 42], &["src/bar.rs".into()], "changed-only");

        // Explicitly flush + close so the file's contents are visible.
        reset_handle_for_tests();

        let text = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = text.trim().split('\n').collect();
        assert_eq!(
            lines.len(),
            2,
            "one JSON line per call; got {}",
            lines.len()
        );

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["sheaf_count"], 3);
        assert_eq!(first["naive_count"], 7);
        assert_eq!(first["scope"], "changed-only");
        let changed = first["changed_files"].as_array().unwrap();
        assert_eq!(changed[0], "src/foo.rs");

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["sheaf_count"], 0);
        assert_eq!(second["naive_count"], 4);

        // Cleanup so we don't leak state into other tests in this module.
        unsafe {
            std::env::remove_var(ABLATION_LOG_ENV);
        }
        reset_handle_for_tests();
    }
}
