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

use leyline_schema::{create_schema, insert_node};

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
    let data = conn.serialize(rusqlite::DatabaseName::Main)?;
    Ok(data.to_vec())
}

/// Project a `pyproject.toml` into an existing connection.
pub fn project_pyproject_into(content: &str, conn: &Connection) -> Result<()> {
    create_schema(conn)?;

    let doc: toml::Value = content.parse().context("invalid TOML")?;
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Root directory
    insert_node(conn, "", "", "", 1, 0, mtime, "")?;

    // /project metadata
    if let Some(project) = doc.get("project").and_then(|v| v.as_table()) {
        insert_node(conn, "project", "", "project", 1, 0, mtime, "")?;

        if let Some(name) = project.get("name").and_then(|v| v.as_str()) {
            insert_node(
                conn,
                "project/name",
                "project",
                "name",
                0,
                name.len() as i64,
                mtime,
                name,
            )?;
        }
        if let Some(version) = project.get("version").and_then(|v| v.as_str()) {
            insert_node(
                conn,
                "project/version",
                "project",
                "version",
                0,
                version.len() as i64,
                mtime,
                version,
            )?;
        }
        if let Some(desc) = project.get("description").and_then(|v| v.as_str()) {
            insert_node(
                conn,
                "project/description",
                "project",
                "description",
                0,
                desc.len() as i64,
                mtime,
                desc,
            )?;
        }
        if let Some(rp) = project.get("requires-python").and_then(|v| v.as_str()) {
            insert_node(
                conn,
                "project/requires-python",
                "project",
                "requires-python",
                0,
                rp.len() as i64,
                mtime,
                rp,
            )?;
        }

        // /deps — project.dependencies
        if let Some(deps) = project.get("dependencies").and_then(|v| v.as_array()) {
            insert_node(conn, "deps", "", "deps", 1, 0, mtime, "")?;
            for raw in deps {
                if let Some(s) = raw.as_str() {
                    let dep = parse_dep(s).with_context(|| format!("parsing dep: {s}"))?;
                    let id = format!("deps/{}", dep.name);
                    insert_node(
                        conn,
                        &id,
                        "deps",
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
            let group_id = group_name.as_str();
            insert_node(conn, group_id, "", group_id, 1, 0, mtime, "")?;
            if let Some(arr) = entries.as_array() {
                for raw in arr {
                    if let Some(s) = raw.as_str() {
                        let dep = parse_dep(s)
                            .with_context(|| format!("parsing {group_name} dep: {s}"))?;
                        let id = format!("{group_id}/{}", dep.name);
                        insert_node(
                            conn,
                            &id,
                            group_id,
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
            insert_node(conn, "optional", "", "optional", 1, 0, mtime, "")?;
            for (extra_name, entries) in opt {
                let extra_id = format!("optional/{extra_name}");
                insert_node(conn, &extra_id, "optional", extra_name, 1, 0, mtime, "")?;
                if let Some(arr) = entries.as_array() {
                    for raw in arr {
                        if let Some(s) = raw.as_str() {
                            let dep = parse_dep(s)
                                .with_context(|| format!("parsing optional dep: {s}"))?;
                            let id = format!("{extra_id}/{}", dep.name);
                            insert_node(
                                conn,
                                &id,
                                &extra_id,
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
    use rusqlite::DatabaseName;
    use std::io::Cursor;

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
        conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        let name: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE id = 'project/name'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "gem");

        let version: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE id = 'project/version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, "0.1.0");

        let rp: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE id = 'project/requires-python'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rp, ">=3.11");
    }

    #[test]
    fn dependencies_normalized() {
        let bytes = project_pyproject(GEM_PYPROJECT).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        // pyyaml → normalized name
        let spec: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE id = 'deps/pyyaml'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spec, ">=6.0.2");

        // google-genai stays hyphenated (PEP 503 normalization)
        let spec: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE id = 'deps/google-genai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spec, ">=1.66.0");

        // Count all deps
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_id = 'deps'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 8);
    }

    #[test]
    fn dev_dependencies() {
        let bytes = project_pyproject(GEM_PYPROJECT).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        let spec: String = conn
            .query_row(
                "SELECT record FROM nodes WHERE id = 'dev/pytest'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spec, ">=9.0.2");
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
        conn.deserialize_read_exact(DatabaseName::Main, Cursor::new(&bytes), bytes.len(), true)
            .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE parent_id = 'optional/security'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
}
