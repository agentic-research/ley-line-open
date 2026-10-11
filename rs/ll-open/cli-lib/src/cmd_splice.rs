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
    let new_source = leyline_ts::splice::splice(&conn, nid, text)?;
    let file_id = leyline_schema::nid_file_id(nid)
        .with_context(|| format!("node path {node:?} names a directory"))?;
    let rel: String = conn
        .query_row(
            "SELECT id FROM _source WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )
        .with_context(|| format!("node path {node:?} has no source file"))?;
    let len = new_source.len();
    // ADR-0040 D3: the caller names the database, so it holds the whole grant.
    crate::source_write::write(&conn, &crate::source_write::Grant::Whole, &rel, new_source)?;
    drop(conn);
    log::info!("Spliced '{node}': source {len} bytes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Cold-parse a tree holding `test.html` = `src` into a fresh on-disk
    /// projection at `db`. The returned directory is the tree root the
    /// arena records, so it must outlive the test.
    fn project_html(db: &Path, src: &[u8]) -> Result<tempfile::TempDir> {
        let tree = tempfile::tempdir()?;
        std::fs::write(tree.path().join("test.html"), src)?;
        let conn = Connection::open(db)?;
        crate::cmd_parse::parse_into_conn(&conn, tree.path(), None, None)?;
        Ok(tree)
    }

    /// Re-open the file from scratch, so what is asserted is what was
    /// COMMITTED to disk rather than what some still-open handle believes.
    fn read_source(db: &Path) -> Result<Vec<u8>> {
        let conn = Connection::open(db)?;
        let file_id: i64 = conn.query_row(
            "SELECT file_id FROM _source WHERE id = 'test.html'",
            [],
            |r| r.get(0),
        )?;
        leyline_ts::splice::source_bytes(&conn, file_id)
    }

    fn read_record(db: &Path, path: &str) -> Result<String> {
        let conn = Connection::open(db)?;
        let nid = leyline_ts::schema::resolve_path(&conn, path)?
            .with_context(|| format!("path must resolve: {path:?}"))?;
        Ok(
            conn.query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |r| {
                r.get(0)
            })?,
        )
    }

    /// `cmd_splice` returns `Result<()>` and writes everything it does into
    /// the database file — so `is_ok()` is worth nothing here, and replacing
    /// the whole body with `Ok(())` is indistinguishable from a successful
    /// splice unless the bytes on disk are read back.
    ///
    /// The assertions cover both halves of what the command promises: the
    /// spliced SOURCE (`splice`) and the re-derived `nodes.record`
    /// (`reproject`). A stub leaves both at their original values.
    #[test]
    fn cmd_splice_rewrites_the_source_and_reprojects_on_disk() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("proj.db");
        let _tree = project_html(&db, b"<p>Hello</p>")?;

        // Pre-state, so a passing assertion below cannot be the state the
        // projection already had.
        assert_eq!(read_source(&db)?, b"<p>Hello</p>");
        assert_eq!(read_record(&db, "test.html/element/text")?, "Hello");

        cmd_splice(&db, "test.html/element/text", "World")?;

        assert_eq!(
            read_source(&db)?,
            b"<p>World</p>",
            "the spliced bytes must be committed to the db on disk",
        );
        assert_eq!(
            read_record(&db, "test.html/element/text")?,
            "World",
            "reprojection must refresh the node record, not just _source",
        );
        Ok(())
    }

    /// projection-v5 addressing boundary: the CLI takes a DISPLAY path and
    /// the projection keys on integer nids, so an unresolvable path has to
    /// fail here — before any bytes move — and say which path it was.
    #[test]
    fn cmd_splice_rejects_a_path_that_does_not_resolve() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("proj.db");
        let _tree = project_html(&db, b"<p>Hello</p>")?;

        let err = cmd_splice(&db, "test.html/no/such/node", "World")
            .expect_err("an unresolvable display path must not splice");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not resolve"),
            "error must name the resolve boundary; got: {msg}",
        );
        assert!(
            msg.contains("test.html/no/such/node"),
            "error must echo the queried path; got: {msg}",
        );
        assert_eq!(
            read_source(&db)?,
            b"<p>Hello</p>",
            "a rejected splice must leave the projection untouched",
        );
        Ok(())
    }
}
