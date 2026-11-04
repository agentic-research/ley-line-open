//! Identity docs say what the code does (bead `ley-line-open-5be950`).
//!
//! Each claim below was stale on origin/main at v0.23.0 and was believed
//! by readers: an external review built an argument on the first one. A
//! claim is checked against the code it describes, not only for its
//! wording, so a doc cannot pass by restating itself.
//!
//! This file `include_str!`s paths outside the crate, which do not resolve in
//! cargo-mutants' scratch build (see `cdc_ownership_docs_test.rs`). Do not
//! route it into a mutation slice.

fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `treeRoot` folds `_tree`, the store's manifest, not `_source`, which has a
/// row only for files the parse projected (ADR-0045 T1).
#[test]
fn tree_root_docs_name_the_table_the_fold_reads() {
    let src = include_str!("../src/tree_root.rs");
    let arch = normalized(include_str!("../../../../docs/ARCHITECTURE.md"));

    // The code: the fold's input is `crate::tree::entries`, the `_tree` reader.
    assert!(
        src.contains("crate::tree::entries(conn)"),
        "tree_root.rs no longer reads _tree through tree::entries; update this test and the docs it guards"
    );

    let module_doc: String = src
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    let module_doc = normalized(&module_doc);
    assert!(
        module_doc.contains("over every `_tree` entry"),
        "tree_root.rs module doc must say the fold is over `_tree` entries"
    );
    assert!(
        !module_doc.contains("over every `_source` row"),
        "tree_root.rs module doc still says the fold is over `_source` rows"
    );
    assert!(
        !src.contains("the arena's _source set folds"),
        "the treeRoot mismatch error must name _tree, the table it recomputed"
    );

    assert!(
        arch.contains("| `_tree` entries and `source_blobs`; `parserId` |"),
        "ARCHITECTURE.md's authority row for treeRoot must name `_tree` as what it depends on"
    );
    assert!(
        !arch.contains("| `_source` rows and `source_blobs`; `parserId` |"),
        "ARCHITECTURE.md's authority row for treeRoot still names `_source` rows"
    );
}

/// The SQLite projection is written beside the capnp segments by one parse; it
/// is not derived from them. The only production reader of a segment hashes it.
#[test]
fn schema_capnp_does_not_claim_sqlite_derives_from_the_log() {
    let lib = normalized(include_str!("../../../ll-core/schema-capnp/src/lib.rs"));
    let parse = include_str!("../src/cmd_parse.rs");

    assert!(
        !lib.contains("SQLite tables are derived from the capnp event log"),
        "schema-capnp's crate doc still claims SQLite is derived from the capnp log"
    );
    assert!(
        lib.contains("The SQLite projection is NOT derived from these records."),
        "schema-capnp's crate doc must state the shipped relation"
    );

    // Tie the claim to code: production reads of a segment are the hash path.
    // If a projection-from-segments path is ever added, this test must be
    // revisited together with the crate doc.
    let production = parse.split("#[cfg(test)]").next().unwrap_or(parse);
    // Calls, not mentions: a doc comment above the fast path names the API.
    // The two calls are `hash_canonical_stream_slow` (Head.rootHash's
    // fallback) and the previous-Head read.
    let reads = production
        .matches("capnp::serialize::read_message(&mut")
        .count();
    assert!(
        reads <= 2,
        "cmd_parse.rs gained a production capnp read ({reads} found); if it projects SQL from the \
         segments, the schema-capnp crate doc's claim must change with it"
    );
}

/// `SourceFile` carries the content hash, never the bytes.
#[test]
fn source_file_schema_does_not_describe_a_bytes_field() {
    let schema = include_str!("../../../ll-core/schema-capnp/schemas/source.capnp");
    let normalized_schema = normalized(schema);

    let fields: Vec<&str> = schema
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains(" @") && l.ends_with(';'))
        .collect();
    assert!(
        fields.iter().all(|f| !f.starts_with("bytes ")),
        "SourceFile gained a `bytes` field; the doc test and its claim must change"
    );
    assert!(
        !normalized_schema.contains("Empty `bytes` if not yet populated"),
        "source.capnp still describes a `bytes` field SourceFile does not have"
    );
    assert!(
        normalized_schema.contains("never the bytes"),
        "source.capnp must say the record carries the hash and not the bytes"
    );
}
