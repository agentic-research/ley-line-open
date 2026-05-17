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
        } => cmd_index(&expand(&db), force, cache_dir),
        Cmd::Query {
            query,
            db,
            k,
            cache_dir,
        } => cmd_query(&expand(&db), &query, k, cache_dir),
        Cmd::Stats { db } => cmd_stats(&expand(&db)),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn cmd_index(db_path: &str, force: bool, cache_dir: Option<PathBuf>) -> Result<()> {
    let mut conn = Connection::open(db_path).with_context(|| format!("open {db_path}"))?;
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
                "INSERT OR REPLACE INTO chat_embeddings(id, embedding, content_preview) VALUES (?, ?, ?)",
            )?;
            for ((sid, _, preview), vec) in batch.iter().zip(vectors.iter()) {
                let normalized = l2_normalize(vec);
                let blob = vec_to_blob(&normalized);
                stmt.execute(params![sid, blob, preview])?;
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

    let mut stmt = conn.prepare("SELECT id, embedding, content_preview FROM chat_embeddings")?;
    let mut scored: Vec<(f32, String, String)> = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let preview: String = row.get(2)?;
            Ok((id, blob, preview))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(id, blob, preview)| {
            let v = blob_to_vec(&blob)?;
            Some((dot(&query_vec, &v), id, preview))
        })
        .collect();
    // partial sort: find top-k by descending score
    scored.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);

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
            content_preview TEXT NOT NULL
         );",
    )?;
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
fn derive_embed_text(record_json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(record_json).ok()?;
    let item = v.get("item")?;
    let title = item.get("title")?.as_str()?.to_string();
    let transcript = item
        .get("transcript")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    // Strip user/assistant role tags so they don't dominate the embedding
    let cleaned = transcript
        .replace("[user] ", "")
        .replace("[assistant] ", "");
    let head = take_chars(&cleaned, MAX_EMBED_CHARS);
    let embed_text = format!("{title} -- {head}");
    let preview = take_chars(&cleaned, PREVIEW_BYTES).replace('\n', " ");
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
    for chunk in b.chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(out)
}

fn expand(p: &str) -> String {
    if let Some(stripped) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}/{}", home.to_string_lossy(), stripped);
    }
    p.to_string()
}
