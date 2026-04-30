//! Inspect command — queries the arena's active SQLite buffer.
//!
//! Default mode: look up a single node by ID and pretty-print it.
//! SQL mode (`--query`): run arbitrary SQL and print tab-separated results.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::{ArenaHeader, Controller};
use memmap2::Mmap;
use rusqlite::{Connection, DatabaseName};

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
    let mmap = unsafe { Mmap::map(&file)? };

    let header: &ArenaHeader =
        bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);

    let file_size = mmap.len() as u64;
    let offset = header
        .active_buffer_offset(file_size)
        .context("invalid arena header (bad magic, version, or active_buffer)")?;
    let buf_size = ArenaHeader::buffer_size(file_size);

    let buf = &mmap[offset as usize..(offset + buf_size) as usize];

    let mut conn = Connection::open_in_memory()?;
    conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(buf), buf.len(), true)
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
        lookup_node(&conn, id)
    }
}

/// Look up a node by ID and pretty-print its columns.
fn lookup_node(conn: &Connection, id: &str) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, name, kind, size FROM nodes WHERE id = ?1",
    )?;

    let exists = stmt.query_row([id], |row| {
        let id: String = row.get(0)?;
        let parent_id: String = row.get(1)?;
        let name: String = row.get(2)?;
        let kind: i64 = row.get(3)?;
        let size: i64 = row.get(4)?;

        let kind_label = if kind == 1 { "dir" } else { "file" };

        println!("id:        {id}");
        println!("parent_id: {parent_id}");
        println!("name:      {name}");
        println!("kind:      {kind} ({kind_label})");
        println!("size:      {size}");

        Ok(())
    });

    match exists {
        Ok(()) => Ok(()),
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
        insert_node(&conn, "real_node", "", "real_node", 1, 0, 0, "").unwrap();

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
}
