//! Inspect command — queries the arena's active SQLite buffer.
//!
//! Default mode: look up a single node by ID and pretty-print it.
//! SQL mode (`--query`): run arbitrary SQL and print tab-separated results.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::mmap::mmap_read;
use leyline_core::{ArenaHeader, Controller};
use rusqlite::Connection;

/// Open the arena's active buffer as a read-only in-memory SQLite connection.
///
/// Steps:
/// 1. Open the Controller to discover the arena path (or use the given arena path directly).
/// 2. mmap the arena file, read the header.
/// 3. Extract the active buffer slice.
/// 4. Deserialize into an in-memory read-only SQLite database.
fn open_arena_db(arena_path: &Path, control_path: Option<&Path>) -> Result<Connection> {
    // If a control path is provided, open the controller and get the arena path from it.
    // Otherwise, use the arena_path directly.
    let resolved_arena_path = if let Some(ctrl_path) = control_path {
        let controller = Controller::open_or_create(ctrl_path)?;
        let p = controller.arena_path();
        if p.is_empty() {
            arena_path.to_path_buf()
        } else {
            std::path::PathBuf::from(p)
        }
    } else {
        arena_path.to_path_buf()
    };

    let file = std::fs::File::open(&resolved_arena_path)
        .with_context(|| format!("open arena file: {}", resolved_arena_path.display()))?;
    let mmap = mmap_read(&file)?;

    let header: &ArenaHeader = bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = header
        .validate_header(file_size)
        .context("arena header validation failed")?;
    let buf_size = ArenaHeader::buffer_size(file_size);

    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    let mut conn = Connection::open_in_memory()?;
    conn.deserialize_read_exact("main", Cursor::new(buf), buf.len(), true)
        .context("sqlite3_deserialize failed")?;

    Ok(conn)
}

/// Execute the inspect command.
///
/// If `query` is Some, runs arbitrary SQL and prints tab-separated results.
/// Otherwise, looks up a single node by `id` and pretty-prints it.
pub fn cmd_inspect(
    id: &str,
    arena: &Path,
    control_path: Option<&Path>,
    query: Option<&str>,
) -> Result<()> {
    let conn = open_arena_db(arena, control_path)?;

    if let Some(sql) = query {
        run_sql(&conn, sql)
    } else {
        println!("{}", lookup_node(&conn, id)?);
        Ok(())
    }
}

/// Look up a node by ID and render its columns as the report `leyline
/// inspect <id>` prints.
///
/// Returns the rendered report rather than printing it. The field set and
/// the `kind` labelling ARE this command's contract — script wrappers and
/// mache tooling read them — and a function that prints and returns `()`
/// puts that contract past the reach of any test in this crate: libtest
/// swallows `println!` at the Rust level, not at fd 1, so the bytes cannot
/// be redirected and read back. Handing the string to the caller is what
/// lets `lookup_node_labels_kind_one_dir_and_kind_zero_file` assert the
/// label instead of merely asserting that the call did not error.
fn lookup_node(conn: &Connection, id: &str) -> Result<String> {
    // The CLI addresses nodes by display path; the projection keys on
    // integer nids (projection-v5). Resolve, then render back.
    let Some(nid) = leyline_schema::resolve_path(conn, id)? else {
        anyhow::bail!("node not found: {id}");
    };
    let mut stmt = conn.prepare("SELECT parent_nid, kind, size FROM nodes WHERE nid = ?1")?;

    let exists = stmt.query_row([nid], |row| {
        let parent_nid: Option<i64> = row.get(0)?;
        let kind: i64 = row.get(1)?;
        let size: i64 = row.get(2)?;
        Ok((parent_nid, kind, size))
    });

    match exists {
        Ok((parent_nid, kind, size)) => {
            let parent_path = match parent_nid {
                Some(p) => leyline_schema::node_path(conn, p)?.unwrap_or_default(),
                None => String::new(),
            };
            let name = id.rsplit_once('/').map(|(_, n)| n).unwrap_or(id);
            let kind_label = if kind == 1 { "dir" } else { "file" };

            Ok(format!(
                "id:        {id}\n\
                 nid:       {nid}\n\
                 parent_id: {parent_path}\n\
                 name:      {name}\n\
                 kind:      {kind} ({kind_label})\n\
                 size:      {size}"
            ))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            anyhow::bail!("node not found: {id}");
        }
        Err(e) => Err(e.into()),
    }
}

/// Run arbitrary SQL and print tab-separated results with column headers.
fn run_sql(conn: &Connection, sql: &str) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let col_count = stmt.column_count();

    // Print header row.
    let headers: Vec<&str> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?"))
        .collect();
    println!("{}", headers.join("\t"));

    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let vals: Vec<String> = (0..col_count)
            .map(|i| row.get::<_, String>(i).unwrap_or_default())
            .collect();
        println!("{}", vals.join("\t"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_schema::{create_schema, insert_node};

    #[test]
    fn cmd_inspect_fails_loudly_when_the_arena_cannot_be_opened() {
        // `cmd_inspect` opens the arena BEFORE it can render anything, so
        // "the arena is not there" must reach the operator as a non-zero
        // exit, not as a silent success that prints nothing. A command that
        // swallows an unopenable arena is worse than one that crashes: the
        // caller (a script wrapper, mache tooling) reads exit 0 and moves on
        // having inspected nothing.
        //
        // This is the only externally observable thing `cmd_inspect` itself
        // does — its success path's output goes through `println!`, which
        // libtest captures at a level fd redirection cannot read back. That
        // is why the rendering lives in `lookup_node`, which returns the
        // report as a String and is asserted directly below.
        let td = tempfile::TempDir::new().unwrap();
        let missing = td.path().join("nope.arena");
        let err = cmd_inspect("some/id", &missing, None, None)
            .expect_err("an unopenable arena must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            !msg.is_empty(),
            "the failure must carry a diagnostic, got an empty message",
        );
    }

    #[test]
    fn lookup_node_errors_with_actionable_message_on_missing_id() {
        // Scale-pin the inspect-CLI error UX. lookup_node is called
        // from `leyline inspect <id>` — at registry scale (50k+ nodes)
        // a typo in the id is the most common mistake. Pin the error
        // message so a refactor doesn't silently change to a less
        // helpful "row not found" / generic SQL error. Clients
        // (script wrappers, mache tooling) parse this string.
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        // Insert one known node so the table exists but the queried
        // id doesn't match.
        let name_id = leyline_schema::intern_name(&conn, "real_node").unwrap();
        insert_node(&conn, 42, Some(-1), Some(name_id), None, 1, 0, 0, 0, "").unwrap();

        let err = lookup_node(&conn, "missing_id").expect_err("must error on missing id");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("node not found"),
            "error must mention 'node not found'; got: {msg}",
        );
        assert!(
            msg.contains("missing_id"),
            "error must echo the queried id for debuggability; got: {msg}",
        );
    }

    /// Mint one v5 leaf FILE node (kind 0) at `rel`, interning its
    /// directory chain — which is what puts a kind 1 DIRECTORY node in
    /// `nodes` for the parent, giving this module both kinds to look up.
    fn mint_leaf_file(conn: &Connection, rel: &str, record: &str) {
        let fid = leyline_schema::ensure_file_id(conn, rel).unwrap();
        let dir = leyline_schema::ensure_dir_nodes(conn, rel, 0).unwrap();
        let fname = rel.rsplit_once('/').map(|(_, n)| n).unwrap_or(rel);
        let name_id = leyline_schema::intern_name(conn, fname).unwrap();
        insert_node(
            conn,
            leyline_schema::file_nid(fid, 0),
            Some(leyline_schema::dir_nid(dir)),
            Some(name_id),
            None,
            0,
            0,
            record.len() as i64,
            1,
            record,
        )
        .unwrap();
    }

    /// `kind` is projected as a raw integer, and the report translates it
    /// for humans: 1 is a directory, everything else is a file. Both arms
    /// have to be looked up for the comparison to be pinned — with only one
    /// node seeded, flipping `==` to `!=` merely swaps which single label is
    /// printed and no single-row assertion that reads the OTHER field can
    /// see it. So this seeds both: `src` (the interned directory, kind 1)
    /// and `src/a.go` (the file node, kind 0), and asserts the label each
    /// one renders. Under the flip, both assertions fail at once.
    #[test]
    fn lookup_node_labels_kind_one_dir_and_kind_zero_file() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        mint_leaf_file(&conn, "src/a.go", "package main");

        let file_report = lookup_node(&conn, "src/a.go").unwrap();
        assert!(
            file_report.contains("kind:      0 (file)"),
            "a kind-0 node must render as 'file'; got:\n{file_report}",
        );

        let dir_report = lookup_node(&conn, "src").unwrap();
        assert!(
            dir_report.contains("kind:      1 (dir)"),
            "a kind-1 node must render as 'dir'; got:\n{dir_report}",
        );
    }

    /// The rest of the report is contract too — `id`, the resolved `nid`,
    /// the rendered `parent_id` display path, `name` and `size` are what
    /// wrappers parse. Pinned here so a whole-function stub returning an
    /// empty report cannot pass as a successful lookup.
    #[test]
    fn lookup_node_renders_every_field_of_the_report() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        mint_leaf_file(&conn, "src/a.go", "package main");

        let nid = leyline_schema::resolve_path(&conn, "src/a.go")
            .unwrap()
            .expect("the minted file node must resolve");
        let report = lookup_node(&conn, "src/a.go").unwrap();

        assert_eq!(
            report,
            format!(
                "id:        src/a.go\n\
                 nid:       {nid}\n\
                 parent_id: src\n\
                 name:      a.go\n\
                 kind:      0 (file)\n\
                 size:      12"
            ),
            "the inspect report is the CLI's parsed contract",
        );
    }
}
