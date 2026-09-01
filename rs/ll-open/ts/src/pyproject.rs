//! Parse `pyproject.toml` and project Python package metadata into the `nodes` table.
//!
//! Projects the dependency graph as a filesystem tree:
//! ```text
//! /                          root
//! /project/name              "gem"
//! /project/version           "0.1.0"
//! /project/requires-python   ">=3.11"
//! /deps/accelerate           ">=1.10.0"
//! /deps/torch                ">=2.8.0"
//! /dev/pytest                ">=9.0.2"
//! ```

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};

use leyline_schema::create_schema;

/// Intern one named tree node in directory nid space (projection-v5, bead
/// `ley-line-open-17c271`): the pyproject projection is a tree of NAMED
/// things — exactly what the `dirs` + `names` interning chain models, the
/// same shape the standalone LSP tree uses. Idempotent per (parent, name);
/// returns the node's `dir_id` (its nid is the negation).
fn named_node(
    conn: &Connection,
    parent_dir_id: i64,
    name: &str,
    kind: i32,
    size: i64,
    mtime: i64,
    record: &str,
) -> Result<i64> {
    let name_id = leyline_schema::intern_name(conn, name)?;
    conn.execute(
        "INSERT OR IGNORE INTO dirs (parent_dir_id, name_id) VALUES (?1, ?2)",
        rusqlite::params![parent_dir_id, name_id],
    )?;
    let dir_id: i64 = conn.query_row(
        "SELECT dir_id FROM dirs WHERE parent_dir_id = ?1 AND name_id = ?2",
        rusqlite::params![parent_dir_id, name_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO nodes (nid, parent_nid, name_id, kind, ord, size, mtime, record) \
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
        rusqlite::params![
            leyline_schema::dir_nid(dir_id),
            leyline_schema::dir_nid(parent_dir_id),
            name_id,
            kind,
            size,
            mtime,
            record
        ],
    )?;
    Ok(dir_id)
}

/// Make sure the root directory's presentation row exists (nid -1).
fn ensure_root_node(conn: &Connection, mtime: i64) -> Result<()> {
    let root_name = leyline_schema::intern_name(conn, "")?;
    conn.execute(
        "INSERT OR IGNORE INTO nodes (nid, parent_nid, name_id, kind, ord, mtime, record) \
         VALUES (-1, NULL, ?1, 1, 0, ?2, '')",
        rusqlite::params![root_name, mtime],
    )?;
    Ok(())
}

/// Parsed dependency with normalized name and version specifier.
struct Dep {
    name: String,
    version_spec: String,
}

fn parse_dep(raw: &str) -> Result<Dep> {
    let req = uv_pep508::Requirement::<uv_pep508::VerbatimUrl>::from_str(raw)
        .map_err(|e| anyhow::anyhow!("bad PEP 508: {e}"))?;
    let name = uv_normalize::PackageName::as_ref(&req.name).to_string();
    let version_spec = match &req.version_or_url {
        Some(uv_pep508::VersionOrUrl::VersionSpecifier(vs)) => vs.to_string(),
        Some(uv_pep508::VersionOrUrl::Url(u)) => u.to_string(),
        None => String::new(),
    };
    Ok(Dep { name, version_spec })
}

/// Project a `pyproject.toml` into the `nodes` table.
///
/// Returns serialized SQLite bytes ready for arena load.
pub fn project_pyproject(content: &str) -> Result<Vec<u8>> {
    let conn = Connection::open_in_memory()?;
    project_pyproject_into(content, &conn)?;
    let data = conn.serialize("main")?;
    Ok(data.to_vec())
}

/// Project a `pyproject.toml` into an existing connection.
pub fn project_pyproject_into(content: &str, conn: &Connection) -> Result<()> {
    create_schema(conn)?;

    // toml 1.x's `Value::FromStr` is stricter about leading content
    // than 0.8 — use the explicit deserializer entry point which is
    // forgiving about leading whitespace + the canonical way to
    // produce a `Value` per the toml crate's 1.x API.
    let doc: toml::Value = toml::from_str(content).context("invalid TOML")?;
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Root directory (dir_id 1, nid -1)
    ensure_root_node(conn, mtime)?;

    // /project metadata
    if let Some(project) = doc.get("project").and_then(|v| v.as_table()) {
        let project_dir = named_node(conn, 1, "project", 1, 0, mtime, "")?;

        for key in ["name", "version", "description", "requires-python"] {
            if let Some(val) = project.get(key).and_then(|v| v.as_str()) {
                named_node(conn, project_dir, key, 0, val.len() as i64, mtime, val)?;
            }
        }

        // /deps — project.dependencies
        if let Some(deps) = project.get("dependencies").and_then(|v| v.as_array()) {
            let deps_dir = named_node(conn, 1, "deps", 1, 0, mtime, "")?;
            for raw in deps {
                if let Some(s) = raw.as_str() {
                    let dep = parse_dep(s).with_context(|| format!("parsing dep: {s}"))?;
                    named_node(
                        conn,
                        deps_dir,
                        &dep.name,
                        0,
                        dep.version_spec.len() as i64,
                        mtime,
                        &dep.version_spec,
                    )?;
                }
            }
        }
    }

    // /dev — dependency-groups.dev
    if let Some(groups) = doc.get("dependency-groups").and_then(|v| v.as_table()) {
        for (group_name, entries) in groups {
            let group_dir = named_node(conn, 1, group_name, 1, 0, mtime, "")?;
            if let Some(arr) = entries.as_array() {
                for raw in arr {
                    if let Some(s) = raw.as_str() {
                        let dep = parse_dep(s)
                            .with_context(|| format!("parsing {group_name} dep: {s}"))?;
                        named_node(
                            conn,
                            group_dir,
                            &dep.name,
                            0,
                            dep.version_spec.len() as i64,
                            mtime,
                            &dep.version_spec,
                        )?;
                    }
                }
            }
        }
    }

    // /optional — project.optional-dependencies
    if let Some(project) = doc.get("project").and_then(|v| v.as_table()) {
        if let Some(opt) = project
            .get("optional-dependencies")
            .and_then(|v| v.as_table())
        {
            let optional_dir = named_node(conn, 1, "optional", 1, 0, mtime, "")?;
            for (extra_name, entries) in opt {
                let extra_dir = named_node(conn, optional_dir, extra_name, 1, 0, mtime, "")?;
                if let Some(arr) = entries.as_array() {
                    for raw in arr {
                        if let Some(s) = raw.as_str() {
                            let dep = parse_dep(s)
                                .with_context(|| format!("parsing optional dep: {s}"))?;
                            named_node(
                                conn,
                                extra_dir,
                                &dep.name,
                                0,
                                dep.version_spec.len() as i64,
                                mtime,
                                &dep.version_spec,
                            )?;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

use std::str::FromStr;

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    /// Resolve a display path and read its record — the v5 read boundary.
    fn record_at(conn: &Connection, path: &str) -> String {
        let nid = leyline_schema::resolve_path(conn, path)
            .unwrap()
            .unwrap_or_else(|| panic!("path must resolve: {path:?}"));
        conn.query_row("SELECT record FROM nodes WHERE nid = ?1", [nid], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// Direct-child count of a display path.
    fn child_count(conn: &Connection, path: &str) -> i64 {
        let nid = leyline_schema::resolve_path(conn, path)
            .unwrap()
            .unwrap_or_else(|| panic!("path must resolve: {path:?}"));
        conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_nid = ?1",
            [nid],
            |r| r.get(0),
        )
        .unwrap()
    }

    const GEM_PYPROJECT: &str = r#"
[project]
name = "gem"
version = "0.1.0"
description = "Self-evaluating mutation loop"
requires-python = ">=3.11"
dependencies = [
    "accelerate>=1.10.0",
    "datasets>=4.0.0",
    "google-genai>=1.66.0",
    "httpx>=0.28.1",
    "pyyaml>=6.0.2",
    "tensorboard>=2.20.0",
    "torch>=2.8.0",
    "transformers>=4.55.2",
]

[dependency-groups]
dev = [
    "pytest>=9.0.2",
]
"#;

    #[test]
    fn project_metadata() {
        let bytes = project_pyproject(GEM_PYPROJECT).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        assert_eq!(record_at(&conn, "project/name"), "gem");
        assert_eq!(record_at(&conn, "project/version"), "0.1.0");
        assert_eq!(record_at(&conn, "project/requires-python"), ">=3.11");
    }

    #[test]
    fn dependencies_normalized() {
        let bytes = project_pyproject(GEM_PYPROJECT).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        // pyyaml → normalized name
        assert_eq!(record_at(&conn, "deps/pyyaml"), ">=6.0.2");

        // google-genai stays hyphenated (PEP 503 normalization)
        assert_eq!(record_at(&conn, "deps/google-genai"), ">=1.66.0");

        // Count all deps
        assert_eq!(child_count(&conn, "deps"), 8);
    }

    #[test]
    fn dev_dependencies() {
        let bytes = project_pyproject(GEM_PYPROJECT).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        assert_eq!(record_at(&conn, "dev/pytest"), ">=9.0.2");
    }

    #[test]
    fn optional_dependencies() {
        let toml = r#"
[project]
name = "example"
version = "1.0.0"
dependencies = ["requests>=2.0"]

[project.optional-dependencies]
security = ["cryptography>=3.0", "pyopenssl>=21.0"]
"#;
        let bytes = project_pyproject(toml).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        assert_eq!(child_count(&conn, "optional/security"), 2);
    }

    #[test]
    fn ensure_root_node_writes_the_root_presentation_row() {
        // `ensure_root_node` returns `Result<()>`, so a body replaced by
        // `Ok(())` is invisible to any caller that only unwraps it. The row
        // at nid -1 IS the effect: it is the mount root every other node in
        // the pyproject projection hangs beneath, so its absence is a tree
        // with no reachable entry point.
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        ensure_root_node(&conn, 4242).unwrap();

        let (parent_nid, kind, ord, mtime, record): (Option<i64>, i32, i64, i64, String) = conn
            .query_row(
                "SELECT parent_nid, kind, ord, mtime, record FROM nodes WHERE nid = -1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .expect("the root presentation row at nid -1 must exist");
        assert_eq!(parent_nid, None, "the root has no parent");
        assert_eq!(kind, 1, "the root is a directory");
        assert_eq!(ord, 0);
        assert_eq!(mtime, 4242, "the caller's mtime must reach the row");
        assert_eq!(record, "");

        // The interned name is the empty string — `resolve_path` renders the
        // root as "" and every child path is relative to it.
        let name: String = conn
            .query_row(
                "SELECT names.text FROM nodes JOIN names USING (name_id) WHERE nodes.nid = -1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "");

        // INSERT OR IGNORE: a second call must not duplicate the row, and the
        // first writer's mtime stands.
        ensure_root_node(&conn, 9999).unwrap();
        let (n, mtime): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(mtime) FROM nodes WHERE nid = -1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "the root row must not duplicate");
        assert_eq!(mtime, 4242, "INSERT OR IGNORE: first writer wins");
    }
}
