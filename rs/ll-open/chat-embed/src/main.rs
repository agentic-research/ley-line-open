//! chat-embed — semantic search over Claude Code chat session databases.
//!
//! Pairs with `mache ingest claude-chats`, which produces a SQLite db in
//! venturi's `results(id, record)` shape (one row per session). This binary
//! adds a `chat_embeddings(id, embedding, content_preview)` sidecar table:
//! 384-dim MiniLM vectors over each session's intent text, plus a brute-force
//! cosine top-k query.
//!
//! 1530 sessions × 1.5KB per vector ≈ 2.3 MB of embeddings — small enough
//! that brute-force cosine in-process is correct first cut. If/when corpus
//! grows past O(100k), graduate to sqlite-vec (vec0 virtual table) or the
//! leyline-fs VectorIndex.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use rusqlite::{Connection, params};

const MODEL_DIMS: usize = 384;
const BATCH_SIZE: usize = 64;
const PREVIEW_BYTES: usize = 280;
// MiniLM tokenizers truncate around 256 tokens (~1000 chars). Stay under
// that on input — extra text is wasted compute.
const MAX_EMBED_CHARS: usize = 1000;

// Stable identifier for the embedding model. Stored on every row so
// query-time can refuse to mix vectors from different models — swapping
// MODEL or fastembed's defaults shifting would otherwise produce silently
// nonsensical cosine scores.
const MODEL_NAME: &str = "minilm-l6-v2-q-384";

#[derive(Parser, Debug)]
#[command(
    name = "chat-embed",
    about = "Index & query Claude Code chat sessions with MiniLM embeddings"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Embed every session in <db> and write to chat_embeddings.
    Index {
        /// Path to mache-produced chats sqlite db.
        #[arg(default_value = "~/.claude/chats.db")]
        db: String,
        /// Re-embed sessions even if a row already exists.
        #[arg(long)]
        force: bool,
        /// Optional fastembed cache dir override.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Top-k cosine-similarity search for a free-text query.
    Query {
        /// Free-text query.
        query: String,
        /// Path to chats db (must already be indexed).
        #[arg(default_value = "~/.claude/chats.db")]
        db: String,
        /// Number of results to return.
        #[arg(short, long, default_value_t = 10)]
        k: usize,
        /// Optional fastembed cache dir override.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Print stats: total sessions, embedded count, missing count.
    Stats {
        #[arg(default_value = "~/.claude/chats.db")]
        db: String,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index {
            db,
            force,
            cache_dir,
        } => cmd_index(&expand(&db)?, force, cache_dir),
        Cmd::Query {
            query,
            db,
            k,
            cache_dir,
        } => cmd_query(&expand(&db)?, &query, k, cache_dir),
        Cmd::Stats { db } => cmd_stats(&expand(&db)?),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn cmd_index(db_path: &str, force: bool, cache_dir: Option<PathBuf>) -> Result<()> {
    let mut conn = Connection::open(db_path).with_context(|| format!("open {db_path}"))?;
    ensure_results_table(&conn)?;
    ensure_embeddings_table(&conn)?;

    let pending: Vec<(String, String, String)> = collect_pending(&conn, force)?;
    if pending.is_empty() {
        log::info!("nothing to embed (use --force to re-embed all)");
        return Ok(());
    }
    log::info!("loading MiniLM (first run downloads ~22MB)…");
    let mut model = load_model(cache_dir)?;

    let mut total_inserted = 0;
    for batch in pending.chunks(BATCH_SIZE) {
        let texts: Vec<&str> = batch.iter().map(|(_, t, _)| t.as_str()).collect();
        let vectors = model.embed(texts, None).context("fastembed embed failed")?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO chat_embeddings(id, embedding, content_preview, model) VALUES (?, ?, ?, ?)",
            )?;
            for ((sid, _, preview), vec) in batch.iter().zip(vectors.iter()) {
                let normalized = l2_normalize(vec);
                let blob = vec_to_blob(&normalized);
                stmt.execute(params![sid, blob, preview, MODEL_NAME])?;
                total_inserted += 1;
            }
        }
        tx.commit()?;
        log::info!("embedded {total_inserted}/{}", pending.len());
    }
    log::info!("done. {total_inserted} embeddings written → {db_path}");
    Ok(())
}

fn cmd_query(db_path: &str, query: &str, k: usize, cache_dir: Option<PathBuf>) -> Result<()> {
    let conn = Connection::open(db_path).with_context(|| format!("open {db_path}"))?;
    ensure_embeddings_table(&conn)?;
    log::info!("loading MiniLM…");
    let mut model = load_model(cache_dir)?;
    let query_vec = model
        .embed(vec![query], None)
        .context("embed query")?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("empty embed result"))?;
    let query_vec = l2_normalize(&query_vec);

    let mut stmt =
        conn.prepare("SELECT id, embedding, content_preview, model FROM chat_embeddings")?;
    let mut wrong_model = 0usize;
    let mut decode_errors = 0usize;
    let mut blob_errors = 0usize;
    // Bounded min-heap: keep only the top-k highest scores. O(N log k)
    // instead of O(N log N) full sort. Invisible at N=1531 but cheap to do
    // right and the corpus grows.
    let mut heap: BinaryHeap<Reverse<ScoredHit>> = BinaryHeap::with_capacity(k + 1);
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = match row.get(0) {
            Ok(v) => v,
            Err(e) => {
                decode_errors += 1;
                log::warn!("skip row: id decode error: {e}");
                continue;
            }
        };
        let blob: Vec<u8> = match row.get(1) {
            Ok(v) => v,
            Err(e) => {
                decode_errors += 1;
                log::warn!("skip row {id}: embedding decode error: {e}");
                continue;
            }
        };
        let preview: String = row.get(2).unwrap_or_default();
        let model: String = row.get(3).unwrap_or_default();
        if model != MODEL_NAME {
            wrong_model += 1;
            continue;
        }
        let Some(v) = blob_to_vec(&blob) else {
            blob_errors += 1;
            log::warn!(
                "skip row {id}: embedding blob length {} != expected {} bytes",
                blob.len(),
                MODEL_DIMS * 4
            );
            continue;
        };
        let hit = ScoredHit {
            score: dot(&query_vec, &v),
            id,
            preview,
        };
        heap.push(Reverse(hit));
        if heap.len() > k {
            heap.pop();
        }
    }
    if wrong_model > 0 {
        log::warn!(
            "skipped {wrong_model} rows from a different model (current: {MODEL_NAME}); re-run `chat-embed index --force` to migrate"
        );
    }
    if decode_errors + blob_errors > 0 {
        log::warn!("skipped {decode_errors} decode errors, {blob_errors} blob shape errors");
    }
    let mut scored: Vec<(f32, String, String)> = heap
        .into_iter()
        .map(|Reverse(h)| (h.score, h.id, h.preview))
        .collect();
    scored.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Enrich with session metadata (title, project, year_month) from the
    // existing results table — gives the query output something useful for
    // a human to act on (or pipe to mache projection).
    for (score, sid, preview) in &scored {
        let (title, project, ym): (String, String, String) = conn
            .query_row(
                "SELECT json_extract(record,'$.item.title'),
                        json_extract(record,'$.item.project'),
                        json_extract(record,'$.item.year_month')
                 FROM results WHERE id=?",
                params![sid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap_or_else(|_| (String::new(), String::new(), String::new()));
        println!("{score:.4}  {sid}");
        println!("        {ym}  {project}");
        println!("        {title}");
        if !preview.is_empty() {
            println!("        > {preview}");
        }
        println!();
    }
    Ok(())
}

fn cmd_stats(db_path: &str) -> Result<()> {
    let conn = Connection::open(db_path).with_context(|| format!("open {db_path}"))?;
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))?;
    let embedded: i64 = conn
        .query_row("SELECT COUNT(*) FROM chat_embeddings", [], |row| row.get(0))
        .unwrap_or(0);
    println!("sessions in results:      {total}");
    println!("sessions with embedding:  {embedded}");
    println!("missing:                  {}", total - embedded);
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn ensure_embeddings_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chat_embeddings (
            id              TEXT PRIMARY KEY,
            embedding       BLOB NOT NULL,
            content_preview TEXT NOT NULL,
            model           TEXT NOT NULL DEFAULT 'unknown'
         );",
    )?;
    // Migrate pre-existing dbs (created before the model column was added)
    // by adding the column with a sentinel value. Query-time filter will skip
    // any 'unknown' rows until they're re-indexed via `chat-embed index --force`.
    let has_model: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('chat_embeddings') WHERE name='model'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if has_model == 0 {
        conn.execute_batch(
            "ALTER TABLE chat_embeddings ADD COLUMN model TEXT NOT NULL DEFAULT 'unknown';",
        )?;
        log::warn!(
            "migrated chat_embeddings: added `model` column (default 'unknown'). Re-run `chat-embed index --force` to attach the current model name to existing rows."
        );
    }
    Ok(())
}

/// Pre-check: friendly error if results table is missing, rather than
/// surfacing rusqlite's "no such table: results" further down the line.
fn ensure_results_table(conn: &Connection) -> Result<()> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='results'",
            [],
            |row| row.get(0),
        )
        .context("probe sqlite_master")?;
    if exists == 0 {
        anyhow::bail!(
            "no `results` table — this db wasn't produced by `mache ingest claude-chats`"
        );
    }
    Ok(())
}

fn load_model(cache_dir: Option<PathBuf>) -> Result<TextEmbedding> {
    let mut opts =
        InitOptions::new(EmbeddingModel::AllMiniLML6V2Q).with_show_download_progress(true);
    if let Some(dir) = cache_dir {
        opts = opts.with_cache_dir(dir);
    }
    TextEmbedding::try_new(opts).context("init fastembed")
}

/// Pull (id, embed_text, preview) tuples for sessions that need work.
/// embed_text is a compact blend of title + transcript head — what we feed
/// the model. preview is a separate, slightly longer human-readable snippet
/// stored next to the vector for query output (saves a join back).
fn collect_pending(conn: &Connection, force: bool) -> Result<Vec<(String, String, String)>> {
    let sql = if force {
        "SELECT id, record FROM results"
    } else {
        "SELECT id, record FROM results
         WHERE id NOT IN (SELECT id FROM chat_embeddings)"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let record: String = row.get(1)?;
        Ok((id, record))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (id, record) = r?;
        if let Some((embed_text, preview)) = derive_embed_text(&record) {
            out.push((id, embed_text, preview));
        }
    }
    Ok(out)
}

/// Build the text we hand to the embedder for one session. Format:
///   "<title> -- <transcript-head>"
/// We bias toward the title (high-signal intent) and the first ~1000 chars
/// of transcript (catches early framing + first assistant response).
///
/// Truncates *before* the role-tag strip so we don't pay O(n) replace on
/// transcripts where only the first 1000 chars survive (matters at scale —
/// some sessions have 100k+ char transcripts).
fn derive_embed_text(record_json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(record_json).ok()?;
    let item = v.get("item")?;
    let title = item.get("title")?.as_str()?.to_string();
    let transcript = item
        .get("transcript")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    // Take enough head to survive role-tag stripping. Role tags are ~10
    // chars each; budget 2× the embed limit so the final cleaned string
    // hits MAX_EMBED_CHARS even after stripping.
    let head_raw = take_chars(transcript, MAX_EMBED_CHARS * 2);
    let cleaned_head = head_raw.replace("[user] ", "").replace("[assistant] ", "");
    let head = take_chars(&cleaned_head, MAX_EMBED_CHARS);
    let embed_text = format!("{title} -- {head}");
    let preview = take_chars(&cleaned_head, PREVIEW_BYTES).replace('\n', " ");
    Some((embed_text, preview))
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn blob_to_vec(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() != MODEL_DIMS * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(MODEL_DIMS);
    for chunk in b.as_chunks::<4>().0 {
        out.push(f32::from_le_bytes(*chunk));
    }
    Some(out)
}

/// Scored result for the top-k heap. Ord compares by score so the
/// `BinaryHeap<Reverse<ScoredHit>>` behaves as a min-heap on score.
#[derive(Debug, Clone)]
struct ScoredHit {
    score: f32,
    id: String,
    preview: String,
}

impl PartialEq for ScoredHit {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}
impl Eq for ScoredHit {}
impl PartialOrd for ScoredHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScoredHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

fn expand(p: &str) -> Result<String> {
    let Some(stripped) = p.strip_prefix("~/") else {
        return Ok(p.to_string());
    };
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!("HOME is unset — pass an absolute db path instead of {p}")
    })?;
    Ok(format!("{}/{}", home.to_string_lossy(), stripped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_unit_length() {
        let v = vec![3.0, 4.0, 0.0];
        let n = l2_normalize(&v);
        let mag: f32 = n.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-6, "expected unit length, got {mag}");
        assert!((n[0] - 0.6).abs() < 1e-6);
        assert!((n[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_zero_is_identity() {
        let v = vec![0.0; 4];
        assert_eq!(l2_normalize(&v), v);
    }

    #[test]
    fn dot_orthogonal_is_zero() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert_eq!(dot(&a, &b), 0.0);
    }

    #[test]
    fn dot_parallel_unit_is_one() {
        let a = l2_normalize(&[1.0, 1.0, 1.0]);
        assert!((dot(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn blob_roundtrip_preserves_vector() {
        let v: Vec<f32> = (0..MODEL_DIMS).map(|i| i as f32 * 0.01).collect();
        let blob = vec_to_blob(&v);
        assert_eq!(blob.len(), MODEL_DIMS * 4);
        let recovered = blob_to_vec(&blob).expect("roundtrip");
        assert_eq!(recovered, v);
    }

    #[test]
    fn blob_to_vec_rejects_wrong_size() {
        assert!(blob_to_vec(&[0u8; 100]).is_none());
        assert!(blob_to_vec(&[0u8; (MODEL_DIMS * 4) - 1]).is_none());
        assert!(blob_to_vec(&[0u8; (MODEL_DIMS * 4) + 1]).is_none());
    }

    #[test]
    fn take_chars_does_not_split_runes() {
        let s = "abc→def→ghi"; // → is 3 UTF-8 bytes
        assert_eq!(take_chars(s, 5), "abc→d");
        assert_eq!(take_chars(s, 100), s);
    }

    /// Combined HOME-env test. env::set_var/remove_var are process-global
    /// and unsafe to interleave with other threads (Rust 2024 made them
    /// `unsafe fn` for this reason). cargo runs tests in parallel by
    /// default, so we exercise the three HOME paths in one sequential test
    /// to avoid races without pulling in a serial_test dep.
    #[test]
    fn expand_handles_tilde_and_home() {
        // No tilde — input passes through regardless of HOME.
        assert_eq!(expand("/abs/path").unwrap(), "/abs/path");
        assert_eq!(expand("rel/path").unwrap(), "rel/path");

        let original = std::env::var_os("HOME");
        // SAFETY: tests in this module that mutate HOME are colocated here
        // and run sequentially within this fn; no other thread we control
        // touches HOME during the test binary lifetime.
        unsafe {
            std::env::set_var("HOME", "/u/test");
        }
        assert_eq!(expand("~/foo").unwrap(), "/u/test/foo");

        unsafe {
            std::env::remove_var("HOME");
        }
        let err = expand("~/foo").unwrap_err().to_string();
        assert!(err.contains("HOME is unset"), "got: {err}");

        // Restore so other tests in the binary see the original value.
        unsafe {
            match original {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn derive_embed_text_strips_role_tags() {
        let record = serde_json::json!({
            "item": {
                "title": "my session",
                "transcript": "[user] hello world

[assistant] hi there"
            }
        })
        .to_string();
        let (embed_text, _) = derive_embed_text(&record).unwrap();
        assert!(embed_text.contains("my session"));
        assert!(embed_text.contains("hello world"));
        assert!(!embed_text.contains("[user]"));
        assert!(!embed_text.contains("[assistant]"));
    }

    #[test]
    fn scored_hit_orders_by_score_ascending_for_min_heap() {
        // BinaryHeap is max-heap; Reverse turns it into min-heap.
        // The smallest score should pop first when wrapped in Reverse.
        let mut heap: BinaryHeap<Reverse<ScoredHit>> = BinaryHeap::new();
        heap.push(Reverse(ScoredHit {
            score: 0.5,
            id: "b".into(),
            preview: String::new(),
        }));
        heap.push(Reverse(ScoredHit {
            score: 0.9,
            id: "a".into(),
            preview: String::new(),
        }));
        heap.push(Reverse(ScoredHit {
            score: 0.1,
            id: "c".into(),
            preview: String::new(),
        }));
        assert_eq!(heap.pop().unwrap().0.id, "c"); // smallest first
    }
}
