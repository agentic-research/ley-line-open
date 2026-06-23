//! Observation table schema — per ADR-0020 §1.
//!
//! One table (`observation`) + two indices, owned by the
//! `SessionObservationPass` and any future observation-emitting pass.
//! Schema is intentionally minimal: a single row per observed event,
//! payload stored inline below `INLINE_THRESHOLD` or hashed into the
//! `BlobStore` above it.
//!
//! This module deliberately lives in `cli-lib` rather than a new crate.
//! Per ADR-0020 §1 the table is one of the smallest stable surfaces in
//! the substrate; spinning up a fourth ll-open crate (`leyline-observation`)
//! is premature until at least a second consumer needs the schema
//! independent of the daemon. When that happens, move this module
//! verbatim into the new crate and re-export from here.
//!
//! ## Idempotency
//!
//! `create_observation_schema` uses `CREATE TABLE IF NOT EXISTS` and
//! `CREATE INDEX IF NOT EXISTS` for both indices so calling it on every
//! pass run is safe. The pattern matches `leyline_hdc::schema::
//! create_hdc_schema` and `leyline_lsp::project::create_lsp_schema`.
//!
//! ## Inline vs hash placement
//!
//! [`INLINE_THRESHOLD`] sits alongside the schema because the column it
//! gates (`payload_inline` vs `payload_hash`) is a schema-shape decision.
//! A future ALTER TABLE adding compression or a hash-algorithm column
//! would land in this same file next to the threshold that determines
//! which column carries the payload.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Inline-vs-hash threshold per ADR-0020 §1. Payloads strictly smaller
/// than this go in `observation.payload_inline` as raw bytes;
/// payloads at-or-above the threshold are written to a [`BlobStore`]
/// and `observation.payload_hash` carries the 32-byte BLAKE3 hash.
///
/// 4096 bytes was chosen because:
///
/// - It is a single SQLite page (default page size). Inline payloads
///   below this stay in the row's first page; row scans don't pay a
///   page fault for the BLOB.
/// - It is large enough that the dominant observation kind
///   (`agent.session_turn`) — typically a few hundred bytes of role-
///   tagged text plus a `mentions` array — fits inline. The hash-
///   fallback path exists for the rare long transcript / pasted code
///   block / multi-file diff observation.
///
/// The threshold is a tunable, not a wire contract (ADR-0020 §1). A
/// future operator can change it; existing rows are unaffected because
/// the column choice is per-row.
///
/// [`BlobStore`]: leyline_core::BlobStore
pub const INLINE_THRESHOLD: usize = 4096;

/// Create the `observation` table + two indices if they don't already
/// exist. Idempotent — safe to call on every pass run.
///
/// **Columns** (per ADR-0020 §1):
/// - `id` — auto-incrementing primary key.
/// - `source` — origin of the observation
///   (`tree-sitter` / `git` / `claude-code` / `agent-edit` / ...).
/// - `payload_kind` — capnp schema name from the typed-payload
///   registry (`agent.session_turn`, `code.symbol_def`, ...).
/// - `payload_inline` — raw payload bytes when smaller than
///   [`INLINE_THRESHOLD`]; `NULL` when the payload was hashed.
/// - `payload_hash` — 32-byte BLAKE3 hash into the [`BlobStore`] when
///   the payload is at-or-above [`INLINE_THRESHOLD`]; `NULL` when
///   inline.
/// - `mentions` — JSON array of stable tokens this observation
///   references. Indexed for `json_each(observation.mentions)`
///   neighborhood queries.
/// - `observed_at` — epoch ms of the **event** (e.g. session-turn
///   timestamp), not the insert time. Used as the watermark for
///   incremental enrichment.
///
/// **Indices**:
/// - `observation_by_kind(payload_kind, observed_at DESC)` — supports
///   `WHERE payload_kind = ? ORDER BY observed_at DESC` for
///   `agreement(token, payload_kind)`.
/// - `observation_by_mentions(mentions)` — supports
///   `json_each(mentions)` lookup for `neighborhood(token, k)`.
///
/// [`BlobStore`]: leyline_core::BlobStore
pub fn create_observation_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;

         CREATE TABLE IF NOT EXISTS observation (
             id             INTEGER PRIMARY KEY,
             source         TEXT NOT NULL,
             payload_kind   TEXT NOT NULL,
             payload_inline BLOB,
             payload_hash   BLOB,
             mentions       TEXT NOT NULL,
             observed_at    INTEGER NOT NULL
         );

         CREATE INDEX IF NOT EXISTS observation_by_kind
             ON observation(payload_kind, observed_at DESC);

         CREATE INDEX IF NOT EXISTS observation_by_mentions
             ON observation(mentions);

         COMMIT;",
    )
    .context("create observation schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert a `sqlite_master` row of `kind` ("table" or "index") and
    /// `name` exists. Mirrors `leyline_hdc::schema::tests::
    /// assert_schema_object_exists` so the two schemas test the same
    /// way and drift between them is visible.
    fn assert_schema_object_exists(conn: &Connection, kind: &str, name: &str) {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type=?1 AND name=?2",
                [kind, name],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "expected {kind} {name} to exist");
    }

    #[test]
    fn inline_threshold_is_4096() {
        // ADR-0020 §1 proposes 4096 bytes. Pin so a careless edit
        // (or a merge-conflict resolution that picked the wrong value)
        // surfaces immediately. When operators want to retune this,
        // update the assertion alongside the documented rationale on
        // the constant.
        assert_eq!(INLINE_THRESHOLD, 4096);
    }

    #[test]
    fn create_observation_schema_is_idempotent() {
        // Both `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT
        // EXISTS` should make repeated invocations safe. The pass
        // calls this on every run, so a non-idempotent statement
        // would surface as a runtime error after the first
        // enrichment cycle.
        let conn = Connection::open_in_memory().unwrap();
        create_observation_schema(&conn).unwrap();
        create_observation_schema(&conn).unwrap();
        create_observation_schema(&conn).unwrap();
    }

    #[test]
    fn create_observation_schema_creates_table_and_indices() {
        let conn = Connection::open_in_memory().unwrap();
        create_observation_schema(&conn).unwrap();

        assert_schema_object_exists(&conn, "table", "observation");
        assert_schema_object_exists(&conn, "index", "observation_by_kind");
        assert_schema_object_exists(&conn, "index", "observation_by_mentions");
    }

    #[test]
    fn observation_columns_match_adr_0020_section_1() {
        // Pin the exact column set + order from ADR-0020 §1. A refactor
        // that renamed a column (e.g. payload_inline → inline_payload)
        // would silently break the SessionObservationPass INSERT
        // statement and every future consumer. The ADR is the wire
        // contract; this test enforces it at the schema level.
        let conn = Connection::open_in_memory().unwrap();
        create_observation_schema(&conn).unwrap();

        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('observation') ORDER BY cid")
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(
            cols,
            vec![
                "id".to_string(),
                "source".to_string(),
                "payload_kind".to_string(),
                "payload_inline".to_string(),
                "payload_hash".to_string(),
                "mentions".to_string(),
                "observed_at".to_string(),
            ],
            "observation column set must match ADR-0020 §1",
        );
    }

    #[test]
    fn observation_payload_columns_are_both_nullable() {
        // Either `payload_inline` is set (small payload) or
        // `payload_hash` is set (large payload) — never both.
        // Schema allows NULL on both so the per-row choice is the
        // pass's job, not a constraint check. Pin so a refactor
        // that added NOT NULL to either column would break the
        // inline-vs-hash dispatch documented on INLINE_THRESHOLD.
        let conn = Connection::open_in_memory().unwrap();
        create_observation_schema(&conn).unwrap();

        // Inline-only row: payload_hash NULL.
        conn.execute(
            "INSERT INTO observation \
             (source, payload_kind, payload_inline, mentions, observed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["test", "kind.a", vec![0u8; 8], "[]", 1i64],
        )
        .unwrap();

        // Hash-only row: payload_inline NULL.
        conn.execute(
            "INSERT INTO observation \
             (source, payload_kind, payload_hash, mentions, observed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["test", "kind.a", vec![0u8; 32], "[]", 2i64],
        )
        .unwrap();
    }
}
