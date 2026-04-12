use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub fn cmd_splice(db: &Path, node: &str, text: &str) -> Result<()> {
    log::info!("Splicing node '{}' in {}", node, db.display());
    let conn = Connection::open(db).with_context(|| format!("open db: {}", db.display()))?;
    let new_source = leyline_ts::splice::splice_and_reproject(&conn, node, text)?;
    drop(conn);
    log::info!("Spliced '{}': source {} bytes", node, new_source.len());
    Ok(())
}
