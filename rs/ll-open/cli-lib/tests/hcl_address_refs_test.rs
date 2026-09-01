//! Raw HCL/Terraform typed-reference regression (ley-line-open-55c1cc).
//!
//! This exercises the same `parse_into_conn` producer used by `leyline parse`,
//! then crosses the serialized SQLite boundary before querying `node_refs`.
//! Mache's schema projector and its retired CGO parser are not involved.

use std::fs;
use std::io::Cursor;

use rusqlite::Connection;
use tempfile::TempDir;

#[test]
fn raw_hcl_serialized_projection_emits_typed_address_refs() {
    let source_dir = TempDir::new().unwrap();
    fs::write(
        source_dir.path().join("main.tf"),
        br#"variable "DATABASE_URL" {
  type        = string
  description = "Database connection string"
  default     = "postgres://localhost:5432/app"
}

module "app" {
  # The source attribute need not be first in the block.
  version = "1.0.0"
  source  = "./modules/app"
}

module "remote_app" {
  source = "github.com/example/terraform-app//modules/service?ref=v1.2.3"
}

module "dynamic_app" {
  source = local.module_source
}

resource "aws_instance" "web" {
  source = "resource-source-must-not-emit"
}

provider "aws" {
  source = "provider-source-must-not-emit"
}

terraform {
  required_providers {
    aws = {
      source = "hashicorp/aws"
    }
  }
}
"#,
    )
    .unwrap();

    let producer = Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&producer, source_dir.path(), Some("hcl"), None)
        .unwrap();
    let bytes = producer.serialize("main").unwrap().to_vec();

    let mut artifact = Connection::open_in_memory().unwrap();
    artifact
        .deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
        .unwrap();

    // projection-v5: `node_refs.source_id` is gone — a ref's file is
    // `nid >> 24`, so the file assertion is a range check against the
    // interned `file_id` of main.tf, and the `_ast` linkage is a
    // PRIMARY KEY equality on nid instead of the (node_id, source_id) pair.
    let file_id = leyline_ts::schema::lookup_file_id(&artifact, "main.tf")
        .unwrap()
        .expect("main.tf must be interned by the parse");
    let (lo, hi) = leyline_ts::schema::file_nid_range(file_id);

    let mut stmt = artifact
        .prepare(
            "SELECT r.token, r.nid, r.container_nid, k.raw_kind
             FROM node_refs AS r
             JOIN _ast AS a ON a.nid = r.nid
             JOIN kinds AS k ON k.kind_id = a.kind_id
             ORDER BY r.token",
        )
        .unwrap();
    let rows: Vec<(String, i64, Option<i64>, String)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();

    let tokens: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "env:DATABASE_URL",
            "mod:./modules/app",
            "mod:github.com/example/terraform-app//modules/service?ref=v1.2.3",
        ]
    );
    for (token, nid, container_nid, node_kind) in rows {
        assert!(
            (lo..=hi).contains(&nid),
            "{token}: node_refs.nid must live in main.tf's nid range"
        );
        assert_eq!(container_nid, None);
        assert_eq!(node_kind, "block");
    }
}
