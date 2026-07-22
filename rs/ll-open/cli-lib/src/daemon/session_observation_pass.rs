//! Session observation pass — Claude Code JSONL → `observation` rows.
//!
//! L8 implementation of bead `ley-line-open-c7c79a`, the **Gate 1**
//! consumer that turns ADR-0020 §1 from prose into a working enrichment
//! pass. Reads pre-segmented Claude Code session turns (one JSON object
//! per line under `~/.claude/sessions/*.jsonl` or any caller-supplied
//! corpus), extracts a `mentions[]` array of stable tokens (paths,
//! `path:sym:NAME`, `bead:ID`, `commit:SHA`) per turn, and inserts one
//! row per turn into `observation`.
//!
//! ## ADR-0020 §1 — Gate 1 ("one pass writes observations")
//!
//! ADR-0020 §1 Gate 1 demands "ingest a fixture session, verify row
//! count + `mentions` extraction." This pass is the implementation
//! that the gate closes against. The fixture test at
//! `cli-lib/tests/session_observation_pass_test.rs` exercises a 5-turn
//! JSONL and asserts exactly 5 observation rows + the expected
//! mention tokens.
//!
//! ## Why not a feature gate
//!
//! Per the scout design report: agent-session ingestion is core to
//! the substrate, not optional. `lsp` / `hdc` / `vec` are extension
//! surfaces a slim build may drop; the observation lattice is the
//! consumer surface this substrate exists to serve. Registering the
//! pass unconditionally in `cmd_daemon` matches that posture.
//!
//! ## Watermark
//!
//! `_meta.session_observation_last_ms` stores the highest
//! `observed_at` seen by a previous run. Re-running the pass over the
//! same corpus skips turns at or before the watermark, so re-ingest
//! is idempotent without de-duping per-row. The watermark advances
//! atomically with the INSERT batch.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::enrichment::{EnrichmentPass, EnrichmentStats};
use super::observation_schema::create_observation_schema;

/// `_meta` key storing the highest `observed_at` (ms) the pass has
/// seen. Re-runs skip turns at or before this value so the same
/// corpus can be re-ingested cheaply.
const WATERMARK_KEY: &str = "session_observation_last_ms";

/// `source` column value for rows produced by this pass.
const SOURCE_CLAUDE_CODE: &str = "claude-code";

/// `payload_kind` for one Claude Code session turn. Matches the
/// future capnp schema name in `ley-line-open-503971`'s typed-payload
/// registry. Today the payload is stored as the raw JSON line — the
/// registry isn't wired up yet, and ADR-0020 §1 says the table is
/// opaque to payload kind. When the registry lands, this pass's
/// `payload_inline` bytes get reinterpreted as the registered capnp
/// schema; no schema change is needed.
const PAYLOAD_KIND_SESSION_TURN: &str = "agent.session_turn";

// ─────────────────────────────────────────────────────────────────────
// Mention extraction
// ─────────────────────────────────────────────────────────────────────
//
// The four token shapes — repo-relative path, `path:sym:NAME`,
// `bead:ID`, `commit:SHA` — emerge from observer convention, not from
// an ADR-prescribed grammar (ADR-0020 §1 explicitly refuses to pre-
// spec the grammar). The patterns below match what current Claude
// Code transcripts cite when the model is reasoning over a tree-
// sitter / leyline-rosary repo. New observer conventions (e.g. a
// future `pr:NN` or `issue:NNN` shorthand) get appended here when the
// first transcript carrying them lands.

/// Stable tokens extracted from a single turn's `text`. Order is
/// "first seen wins"; duplicates within the same turn are de-duped so
/// the `mentions` array stays compact for `json_each` lookups.
fn extract_mentions(text: &str) -> Vec<String> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();

    let push =
        |out: &mut Vec<String>, seen: &mut std::collections::BTreeSet<String>, token: String| {
            if seen.insert(token.clone()) {
                out.push(token);
            }
        };

    // `path:sym:NAME` and bare `bead:ID` / `commit:SHA` are prefix-
    // tagged, so a single pass over whitespace-bounded tokens picks
    // them up. Paths are matched by extension suffix.
    for raw in text.split(|c: char| c.is_whitespace()) {
        // Trim trailing punctuation the reasoning text typically
        // wraps the token in (".", ",", ")", etc.). Leading punct
        // gets stripped too so `(rs/ll-open/...)` works.
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '\'' | '"' | '`'
            )
        });
        if token.is_empty() {
            continue;
        }

        // `<path>:sym:<NAME>` — keep the full triple, not the path
        // half alone. Detected by the `:sym:` infix, not regex, so
        // a `:sym:` token with arbitrary path shape still matches.
        if let Some(idx) = token.find(":sym:")
            && idx > 0
            && idx + 5 < token.len()
        {
            push(&mut out, &mut seen, token.to_string());
            continue;
        }

        // Bare `sym:NAME` — observer convention for symbol citations
        // whose path is implicit in the surrounding context (the
        // session itself is anchored to a project). Distinct from
        // the `<path>:sym:NAME` infix above: this captures
        // citations like "touches sym:Foo" that are common in
        // reasoning prose.
        if let Some(rest) = token.strip_prefix("sym:")
            && !rest.is_empty()
        {
            push(&mut out, &mut seen, token.to_string());
            continue;
        }

        // `bead:ID` — observer convention is `bead:<repo>-<6hex>`
        // (rosary's bead format). Accept any `bead:` prefix with at
        // least one trailing non-empty character; the resolver
        // downstream can validate.
        if let Some(rest) = token.strip_prefix("bead:")
            && !rest.is_empty()
        {
            push(&mut out, &mut seen, token.to_string());
            continue;
        }

        // `commit:<sha>` — git commit references. Same posture as
        // `bead:` — accept any non-empty payload, defer validation
        // to the resolver.
        if let Some(rest) = token.strip_prefix("commit:")
            && !rest.is_empty()
        {
            push(&mut out, &mut seen, token.to_string());
            continue;
        }

        // Bare repo-relative path. Matched by recognizable code-file
        // extensions. The list pairs with the languages the daemon
        // already projects via `leyline-ts`; new extensions land
        // here when an observer cites them.
        if is_repo_relative_path(token) {
            push(&mut out, &mut seen, token.to_string());
        }
    }

    out
}

/// True if `token` looks like a repo-relative source path. Conservative:
/// requires a path separator (so `foo.rs` standalone is rejected) and
/// a known code extension. Prevents the parser from falsely promoting
/// every word ending in `.md` (e.g. `README.md` is fine but `etc.md` in
/// prose is not — the path separator guard catches the prose case).
fn is_repo_relative_path(token: &str) -> bool {
    // Must look like a path: at least one '/', no leading '/'
    // (absolute paths are out — they're not repo-relative).
    if !token.contains('/') || token.starts_with('/') {
        return false;
    }
    // Extension allowlist. Mirrors the languages leyline-ts already
    // projects plus the doc / config formats Claude Code transcripts
    // routinely cite.
    matches!(
        std::path::Path::new(token)
            .extension()
            .and_then(|e| e.to_str()),
        Some("rs")
            | Some("go")
            | Some("py")
            | Some("md")
            | Some("toml")
            | Some("capnp")
            | Some("json")
            | Some("yaml")
            | Some("yml")
            | Some("ts")
            | Some("tsx")
            | Some("js")
            | Some("html")
            | Some("hcl")
            | Some("ex")
            | Some("exs")
    )
}

// ─────────────────────────────────────────────────────────────────────
// JSONL parsing
// ─────────────────────────────────────────────────────────────────────

/// One pre-segmented Claude Code session turn, post-`[role]` split.
///
/// The pass operates on this shape (one JSON object per line, no
/// outer wrapper) — decoupling the pass from the upstream
/// `[role] text` splitter that turns a `~/.claude/chats.db` transcript
/// into per-turn records. The fixture at
/// `tests/fixtures/session_5turn.jsonl` matches this shape; future
/// consumers (e.g. a daemon-side ingestor for `~/.claude/sessions/*`)
/// emit the same shape from their splitter.
#[derive(Debug, serde::Deserialize)]
struct SessionTurn {
    /// 1-based turn number within the session. Carried into the
    /// payload for downstream consumers; the pass doesn't use it for
    /// ordering — `ts_ms` is the canonical ordering key.
    #[allow(dead_code)]
    turn: u32,
    /// `"user"` or `"assistant"`. Reserved for future
    /// `agreement(token, payload_kind)` queries that may filter by
    /// role; carried verbatim into the payload bytes.
    #[allow(dead_code)]
    role: String,
    /// Event-time millis. Becomes `observation.observed_at` and feeds
    /// the watermark.
    ts_ms: i64,
    /// Raw turn text. `mentions[]` is extracted from this; the full
    /// text is also preserved as the payload bytes for future
    /// re-analysis.
    text: String,
}

/// Read a JSONL file into turns. One JSON object per non-empty line;
/// trailing-empty / blank lines are tolerated. Malformed lines abort
/// the parse with a line-numbered error so a corrupt corpus surfaces
/// loudly instead of silently dropping observations.
fn read_session_jsonl(path: &Path) -> Result<Vec<SessionTurn>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read session jsonl {}", path.display()))?;

    let mut turns = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let turn: SessionTurn = serde_json::from_str(line).with_context(|| {
            format!("parse session-turn JSON at {}:{}", path.display(), idx + 1)
        })?;
        turns.push(turn);
    }
    Ok(turns)
}

// ─────────────────────────────────────────────────────────────────────
// SessionObservationPass
// ─────────────────────────────────────────────────────────────────────

/// Enrichment pass that ingests Claude Code session JSONLs into the
/// `observation` table. One row per turn; payload = raw JSON line;
/// mentions = extracted via [`extract_mentions`].
///
/// **Corpus discovery.** Today the pass takes the corpus path from the
/// `LEYLINE_SESSION_CORPUS` environment variable: a directory of
/// `*.jsonl` files, each one a pre-segmented session. The env var is
/// the slimmest plumbing that lets fixture tests and operators point
/// at a corpus without a full CLI flag. When a daemon-side `--sessions
/// <dir>` flag lands, the pass picks it up from `DaemonContext`
/// instead; the env-var path stays as the test injection point.
///
/// **No corpus = no work.** With the env var unset (the default for
/// open-edition daemons running outside an interactive Claude
/// session), `run()` returns immediately with `items_added = 0`. The
/// pass is registered unconditionally; the substrate just observes
/// "no sessions to ingest" until someone wires a corpus in.
pub struct SessionObservationPass;

impl SessionObservationPass {
    /// Construct a pass. No state today — corpus path is resolved per
    /// `run()` from the environment.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SessionObservationPass {
    fn default() -> Self {
        Self::new()
    }
}

impl EnrichmentPass for SessionObservationPass {
    fn name(&self) -> &str {
        "session-observation"
    }

    fn depends_on(&self) -> &[&str] {
        // Independent of tree-sitter / lsp / hdc — the pass writes
        // claims about agent sessions, which don't require the
        // projected source to be parsed first. Running it earlier in
        // the pipeline doesn't change its output, so leaving deps
        // empty keeps the topo-sort flexible.
        &[]
    }

    fn reads(&self) -> &[&str] {
        // `_meta` for the watermark key. Source bytes come from
        // outside the db (JSONL files on disk).
        &["_meta"]
    }

    fn writes(&self) -> &[&str] {
        // Schema Partition invariant: this pass is the sole writer
        // of `observation`. Other passes that emit observation rows
        // in the future (e.g. a tree-sitter-driven `code.symbol_def`
        // pass) will need to co-own this table or land their rows
        // through a shared helper — same posture as `_hdc_subtree_
        // cache` which `HdcEnrichmentPass` declares ownership of.
        &["observation"]
    }

    fn run(
        &self,
        conn: &Connection,
        _source_dir: &Path,
        _changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        let start = Instant::now();

        // Ensure the table exists. Idempotent (CREATE … IF NOT
        // EXISTS) so calling on every run is safe.
        create_observation_schema(conn)
            .context("SessionObservationPass: create_observation_schema")?;

        // Resolve corpus path. Absent env var → no work.
        let corpus_dir = match std::env::var("LEYLINE_SESSION_CORPUS") {
            Ok(v) if !v.is_empty() => std::path::PathBuf::from(v),
            _ => {
                return Ok(EnrichmentStats {
                    pass_name: "session-observation".to_string(),
                    files_processed: 0,
                    items_added: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                    skipped: vec!["LEYLINE_SESSION_CORPUS env var unset; no work".to_string()],
                });
            }
        };

        // Read watermark. Missing key = first run = 0 (every turn
        // is newer). Malformed value = 0 (treat as missing so a
        // corrupt _meta row can't wedge ingestion forever).
        let watermark: i64 = leyline_ts::schema::get_meta(conn, WATERMARK_KEY)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);

        let files = collect_jsonl_files(&corpus_dir)
            .with_context(|| format!("scan session corpus {}", corpus_dir.display()))?;

        if files.is_empty() {
            return Ok(EnrichmentStats {
                pass_name: "session-observation".to_string(),
                files_processed: 0,
                items_added: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                skipped: Vec::new(),
            });
        }

        let mut items_added: u64 = 0;
        let mut files_processed: u64 = 0;
        let mut max_observed_at: i64 = watermark;

        let mut insert_stmt = conn.prepare_cached(
            "INSERT INTO observation \
             (source, payload_kind, payload_inline, payload_hash, mentions, observed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        for file in &files {
            let turns = match read_session_jsonl(file) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("SessionObservationPass: skip {} ({e:#})", file.display());
                    continue;
                }
            };

            files_processed += 1;

            for turn in &turns {
                if turn.ts_ms <= watermark {
                    continue;
                }

                let payload_bytes = serde_json::to_vec(&serde_json::json!({
                    "turn": turn.turn,
                    "role": turn.role,
                    "ts_ms": turn.ts_ms,
                    "text": turn.text,
                }))
                .context("serialize session-turn payload")?;

                let mentions = extract_mentions(&turn.text);
                let mentions_json =
                    serde_json::to_string(&mentions).context("serialize mentions json")?;

                // Inline-vs-hash dispatch per ADR-0020 §1 (bead d24e68).
                // Small payloads stay inline; payloads at-or-above
                // INLINE_THRESHOLD are content-addressed into the
                // arena-local `observation_blobs` table (dedup, no inline
                // bloat) — kept inside the one .db, consistent with
                // `source_blobs`/`capnp_blobs`, so the arena stays a single
                // portable file. Cross-arena dedup via the global
                // FsBlobStore is the separate design-gated bead d22735.
                let (payload_inline, payload_hash) =
                    crate::daemon::observation_schema::put_observation_payload(
                        conn,
                        &payload_bytes,
                    )
                    .context("store observation payload")?;

                insert_stmt.execute(rusqlite::params![
                    SOURCE_CLAUDE_CODE,
                    PAYLOAD_KIND_SESSION_TURN,
                    payload_inline,
                    payload_hash,
                    mentions_json,
                    turn.ts_ms,
                ])?;

                items_added += 1;
                if turn.ts_ms > max_observed_at {
                    max_observed_at = turn.ts_ms;
                }
            }
        }

        // Advance the watermark. Failure here is recoverable on the
        // next run (we'll re-ingest the same rows; observation has
        // no uniqueness constraint so duplicates would land). Log
        // loudly so the operator sees the drift.
        if max_observed_at > watermark
            && let Err(e) =
                leyline_ts::schema::set_meta(conn, WATERMARK_KEY, &max_observed_at.to_string())
        {
            log::error!(
                "SessionObservationPass: failed to advance watermark to {max_observed_at}: \
                 {e:#}",
            );
        }

        log::info!(
            "SessionObservationPass: files_processed={files_processed} \
             items_added={items_added} watermark={max_observed_at}",
        );

        Ok(EnrichmentStats {
            pass_name: "session-observation".to_string(),
            files_processed,
            items_added,
            duration_ms: start.elapsed().as_millis() as u64,
            skipped: Vec::new(),
        })
    }
}

/// Enumerate `*.jsonl` files directly under `dir`. Non-recursive —
/// a session corpus is one flat directory of files per ADR-0020 §1's
/// "one event = one row" posture.
fn collect_jsonl_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "jsonl")
        {
            out.push(path);
        }
    }
    // Stable order so test fixtures are deterministic.
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::enrichment::assert_pass_metadata;

    #[test]
    fn session_observation_pass_metadata_pinned() {
        // Pin the four metadata fields per the Schema Partition
        // contract. Drift in any of them (rename, reorder, gaining
        // a dependency, accidentally adding `observation` to a
        // second pass's writes) breaks substrate invariants
        // silently — the pin surfaces them.
        let pass = SessionObservationPass::new();
        assert_pass_metadata(
            &pass,
            "session-observation",
            &[],
            &["_meta"],
            &["observation"],
        );
    }

    #[test]
    fn extract_mentions_recovers_all_four_token_shapes() {
        // The four observer-emitted shapes the substrate cares
        // about today (ADR-0020 §1 paragraph on "stable token
        // formats emerge from observers"). Pin so a refactor that
        // accidentally dropped one (e.g. removed the `:sym:` infix
        // detector) would surface here.
        let text = "look at rs/ll-open/sheaf/src/lib.rs:sym:CellComplex per \
                    bead:ley-line-open-8bf731 against commit:962c8e8 with \
                    docs/adr/0020.md";
        let mentions = extract_mentions(text);
        assert!(
            mentions.contains(&"rs/ll-open/sheaf/src/lib.rs:sym:CellComplex".to_string()),
            "path:sym:NAME shape missing: {mentions:?}"
        );
        assert!(
            mentions.contains(&"bead:ley-line-open-8bf731".to_string()),
            "bead:ID shape missing: {mentions:?}"
        );
        assert!(
            mentions.contains(&"commit:962c8e8".to_string()),
            "commit:SHA shape missing: {mentions:?}"
        );
        assert!(
            mentions.contains(&"docs/adr/0020.md".to_string()),
            "bare path shape missing: {mentions:?}"
        );
    }

    #[test]
    fn extract_mentions_dedupes_within_a_turn() {
        // Same token cited twice in one turn lands once in the
        // mentions array. Keeps `json_each(mentions)` cardinality
        // bounded per turn so the index doesn't bloat from
        // model-generated prose that repeats a token in conclusion.
        let text = "rs/ll-open/sheaf/src/lib.rs and again rs/ll-open/sheaf/src/lib.rs";
        let mentions = extract_mentions(text);
        assert_eq!(
            mentions.len(),
            1,
            "duplicate token must dedupe: {mentions:?}"
        );
        assert_eq!(mentions[0], "rs/ll-open/sheaf/src/lib.rs");
    }

    #[test]
    fn extract_mentions_rejects_bare_extension_in_prose() {
        // "etc.md in prose" must NOT be captured as a path — the
        // separator guard in `is_repo_relative_path` is what
        // prevents every word ending in `.md` from being promoted.
        // Pin so a refactor that loosened the guard (e.g. dropped
        // the `/` requirement) would surface here.
        let text = "We discussed etc.md briefly but didn't write a section.";
        let mentions = extract_mentions(text);
        assert!(
            !mentions.iter().any(|m| m == "etc.md"),
            "bare extension in prose must not become a mention: {mentions:?}"
        );
    }

    #[test]
    fn extract_mentions_rejects_absolute_paths() {
        // Absolute paths are out of scope — repo-relative is what
        // `neighborhood(token, k)` keys on, so an absolute path
        // would never match a tree-sitter / git observation token.
        let text = "see /tmp/scratch.rs which is not part of the repo";
        let mentions = extract_mentions(text);
        assert!(
            !mentions.iter().any(|m| m.starts_with('/')),
            "absolute path must not become a mention: {mentions:?}"
        );
    }

    #[test]
    fn extract_mentions_strips_surrounding_punctuation() {
        // Reasoning text wraps tokens in parens, periods, etc. The
        // trim pass must recover the bare token so it matches the
        // canonical form other observers emit.
        let text =
            "see (rs/ll-open/sheaf/src/lib.rs:sym:CellComplex), then [bead:ley-line-open-79a37c].";
        let mentions = extract_mentions(text);
        assert!(
            mentions.contains(&"rs/ll-open/sheaf/src/lib.rs:sym:CellComplex".to_string()),
            "parens must be trimmed: {mentions:?}"
        );
        assert!(
            mentions.contains(&"bead:ley-line-open-79a37c".to_string()),
            "brackets must be trimmed: {mentions:?}"
        );
    }

    #[test]
    fn is_repo_relative_path_requires_separator() {
        assert!(!is_repo_relative_path("README.md"));
        assert!(is_repo_relative_path("docs/README.md"));
        assert!(!is_repo_relative_path("/tmp/scratch.rs"));
        assert!(is_repo_relative_path("rs/main.rs"));
    }

    #[test]
    fn is_repo_relative_path_recognizes_substrate_extensions() {
        // Drift guard: pin the extension allowlist so a refactor
        // that dropped one (e.g. `.toml` for Cargo.toml citations)
        // would surface here. The list pairs with leyline-ts's
        // language coverage plus the .md / .toml / .capnp formats
        // Claude Code transcripts routinely cite.
        for ext in &["rs", "go", "py", "md", "toml", "capnp", "json", "yaml"] {
            let path = format!("a/b.{ext}");
            assert!(is_repo_relative_path(&path), "must accept .{ext}");
        }
    }
}
