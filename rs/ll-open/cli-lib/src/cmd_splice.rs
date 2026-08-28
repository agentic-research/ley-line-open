use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub fn cmd_splice(db: &Path, node: &str, text: &str) -> Result<()> {
    log::info!("Splicing node '{}' in {}", node, db.display());
    let conn = Connection::open(db).with_context(|| format!("open db: {}", db.display()))?;
    // The CLI addresses nodes by display path; the projection keys on
    // integer nids (projection-v5) — resolve at this boundary.
    let nid = leyline_ts::schema::resolve_path(&conn, node)?
        .with_context(|| format!("node path {node:?} does not resolve in this projection"))?;
    let new_source = leyline_ts::splice::splice_and_reproject(&conn, nid, text)?;
    drop(conn);
    log::info!("Spliced '{}': source {} bytes", node, new_source.len());
    Ok(())
}
