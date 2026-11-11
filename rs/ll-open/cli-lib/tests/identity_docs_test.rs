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

/// The prose of a comment block, line markers (`#`, `//!`) stripped, so a
/// claim that wraps across lines still reads as one sentence.
fn comment_prose(s: &str) -> String {
    let lines: Vec<&str> = s
        .lines()
        .map(|l| {
            let l = l.trim_start();
            l.strip_prefix("//!")
                .or_else(|| l.strip_prefix('#'))
                .unwrap_or(l)
        })
        .collect();
    normalized(&lines.join(" "))
}

/// `storeRoot` folds `_tree`, the store's manifest, not `_source`, which has
/// a row only for files the parse projected (ADR-0045 T1), and folds no
/// parser input (ADR-0047 law C7).
#[test]
fn store_root_docs_name_the_table_the_fold_reads() {
    let src = include_str!("../src/store_root.rs");
    let arch = normalized(include_str!("../../../../docs/ARCHITECTURE.md"));

    // The code: the fold's input is `crate::tree::entries`, the `_tree` reader.
    assert!(
        src.contains("crate::tree::entries(conn)"),
        "store_root.rs no longer reads _tree through tree::entries; update this test and the docs it guards"
    );

    let module_doc: String = src
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    let module_doc = comment_prose(&module_doc);
    assert!(
        module_doc.contains("over every `_tree` entry"),
        "store_root.rs module doc must say the tree is over `_tree` entries"
    );
    assert!(
        !module_doc.contains("over every `_source` row"),
        "store_root.rs module doc still says the fold is over `_source` rows"
    );
    assert!(
        module_doc.contains("no parser input"),
        "store_root.rs module doc must say storeRoot folds no parser input"
    );

    assert!(
        arch.contains("| `_tree` entries and `source_blobs`; no parser input |"),
        "ARCHITECTURE.md's authority row for storeRoot must name `_tree` and no parser input"
    );
    assert!(
        !arch.contains("| `_source` rows and `source_blobs`; `parserId` |"),
        "ARCHITECTURE.md's authority row still names `_source` rows"
    );
}

/// `node_hash` is merkle-ast-v3, keyed under `leyline/ast-node/v3` (ADR-0047
/// D-i, bead `ley-line-open-73add7`), and `node_content` holds only what it
/// determines (D-iii, `ley-line-open-b5d354`). The docs a second
/// implementation or a SQL consumer reads say so, checked against the code.
#[test]
fn node_hash_docs_name_the_keyed_construction() {
    let src = include_str!("../src/cmd_parse.rs");
    let ddl = include_str!("../../ts/src/schema.rs");
    let sigma = normalized(include_str!(
        "../../../ll-core/schema-spec/sigma/v1/README.md"
    ));
    let adr = normalized(include_str!(
        "../../../../docs/adr/0047-sigma-amendment-constructions-witnesses-edges.md"
    ));
    let contract = normalized(include_str!("../../../../docs/TABLE_CONTRACT.md"));

    // The code: one keyed context, both folds keyed, no in-band prefix.
    for code in [
        "const NODE_HASH_CONTEXT: &str = \"leyline/ast-node/v3\";",
        "*leyline_core::substrate::derive_key(NODE_HASH_CONTEXT, &p).as_bytes()",
    ] {
        assert!(
            src.contains(code),
            "cmd_parse.rs changed `{code}`; update the node_hash docs with it"
        );
    }
    assert!(
        !src.contains("NODE_HASH_DOMAIN"),
        "cmd_parse.rs still carries the merkle-ast-v2 in-band prefix"
    );
    assert!(
        src.matches("substrate::derive_key(NODE_HASH_CONTEXT, &p)")
            .count()
            == 2,
        "both the leaf and the internal fold must be keyed"
    );

    assert!(
        sigma
            .contains("`node_hash` v3 (`merkle-ast-v3`), `derive_key(\"leyline/ast-node/v3\", …)`"),
        "sigma/v1/README.md's astNode row must name the keyed construction"
    );
    assert!(
        adr.contains(
            "| `node_hash` | merkle-ast-v3: `derive_key(\"leyline/ast-node/v3\", tag ‖ …)`"
        ) && adr.contains("**Complies** (M2)"),
        "ADR-0047 §3's node_hash row must record the keyed construction as shipped"
    );

    // node_content's columns, in the DDL and in the contract.
    let table = ddl
        .split("CREATE TABLE IF NOT EXISTS node_content (")
        .nth(1)
        .and_then(|rest| rest.split(");").next())
        .expect("schema.rs declares node_content");
    for gone in ["raw_kind", "lang"] {
        assert!(
            !table.contains(gone),
            "node_content's DDL has `{gone}` again; it is not a function of node_hash"
        );
    }
    assert!(
        contract.contains(
            "`node_tag`, `kind`, `token`, `arity`. Every column is a function of `node_hash`"
        ),
        "TABLE_CONTRACT.md's node_content row must list the v12 columns"
    );
}

/// head.capnp is the cross-runtime description of `storeRoot`: another
/// runtime implements the fold from it. It must describe the directory
/// nodes, the git mode in `b`, empty params and empty directories as the code
/// does, say the signature covers it, which `head_digest` does, and say the
/// retired `treeRoot` is refused.
#[test]
fn head_schema_describes_the_store_root_the_code_computes() {
    let schema = comment_prose(include_str!(
        "../../../ll-core/schema-capnp/schemas/head.capnp"
    ));
    let src = include_str!("../src/store_root.rs");
    let digest = include_str!("../../../ll-core/core/src/head_digest.rs");

    for claim in [
        "over the store's manifest `_tree`",
        "scheme `leyline/store-tree/v1`, params EMPTY, `canonVersion` 1",
        "(BLAKE3(name bytes), 2i, byte length of the name)",
        "(child, 2i+1, git mode of the child)",
        "An empty directory is the fold of no entries.",
        "SIGNED: the head signature's digest (`head_digest`, context",
        "\"leyline head digest v3\") binds `storeRoot` and `parserId` separately",
        "retiredTreeRoot @7",
        "HeadFormatError::RetiredDigestV2",
    ] {
        assert!(
            schema.contains(claim),
            "head.capnp's storeRoot description is missing: {claim}"
        );
    }

    // Tie the description to the code it describes: the scheme, empty
    // params, the git mode as `b`, the directory mode for a child directory.
    for code in [
        "pub const STORE_TREE_SCHEME: &str = \"leyline/store-tree/v1\";",
        "params: Vec::new(),",
        "b: u64::from(mode),",
        "push_child(&mut top.entries, name, *blob, mode.git());",
        "crate::tree::DIRECTORY_MODE,",
    ] {
        assert!(
            src.contains(code),
            "store_root.rs changed `{code}`; update head.capnp's storeRoot description with it"
        );
    }
    // "SIGNED" is true only while the head digest binds storeRoot and
    // parserId. If either leaves the preimage, this fails and the schema text
    // must change with it.
    for bound in [
        "hasher.update(store_root.as_bytes());",
        "hasher.update(parser_id.as_bytes());",
        "pub const HEAD_DIGEST_CONTEXT: &str = \"leyline head digest v3\";",
    ] {
        assert!(
            digest.contains(bound),
            "head_digest no longer binds `{bound}`; update head.capnp's SIGNED note"
        );
    }
}

/// `Head.unboundFacts` is the producer's count, `node_refs` rows whose token
/// has no `node_defs` row; head.capnp once described `fact_edges` rows with a
/// NULL `dst` under a "W5" ratchet that does not exist (bead
/// `ley-line-open-cc2130`).
#[test]
fn head_schema_describes_the_unbound_facts_the_code_counts() {
    let schema = include_str!("../../../ll-core/schema-capnp/schemas/head.capnp");
    let field = schema
        .find("unboundFacts @4")
        .expect("head.capnp declares unboundFacts @4");
    // The field's own comment: the block between the previous field and it.
    let start = schema[..field]
        .rfind("segmentBytes @3")
        .expect("segmentBytes precedes unboundFacts");
    let doc = comment_prose(&schema[start..field]);
    for claim in [
        "`node_refs` rows whose `token` has no `node_defs` row",
        "`_meta.unbound_facts`",
        "extraction epoch",
    ] {
        assert!(
            doc.contains(claim),
            "head.capnp's unboundFacts description is missing: {claim}"
        );
    }
    for stale in ["fact_edges", "W5", "ratchet gate", "<= baseline"] {
        assert!(
            !doc.contains(stale),
            "head.capnp's unboundFacts description names `{stale}`"
        );
    }
    // Tie the description to the producer and the table contract.
    let parse = include_str!("../src/cmd_parse.rs");
    assert!(
        parse.contains(
            "SELECT count(*) FROM node_refs WHERE token NOT IN (SELECT token FROM node_defs)"
        ),
        "cmd_parse changed its unbound_facts count; update head.capnp's unboundFacts"
    );
    let contract = normalized(include_str!("../../../../docs/TABLE_CONTRACT.md"));
    assert!(
        contract.contains(
            "`unbound_facts` = the count of `node_refs` rows whose token has no `node_defs` row"
        ),
        "TABLE_CONTRACT's unbound_facts definition moved; update head.capnp's unboundFacts"
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

/// AGENTS.md's identity vocabulary and the review kit's authority ledger
/// name each construction's context or scheme; every one must be the string
/// the code keys with (bead `ley-line-open-a678c2`). `run_id` is still the
/// v1 prefix-in-preimage construction, so both must say v2 is proposed.
#[test]
fn identity_vocabulary_names_the_contexts_the_code_keys_with() {
    let agents = normalized(include_str!("../../../../AGENTS.md"));
    let kit = normalized(include_str!(
        "../../../../.claude/skills/leyline-review-kit/SKILL.md"
    ));
    let parse = include_str!("../src/cmd_parse.rs");
    let store = include_str!("../src/store_root.rs");
    let digest = include_str!("../../../ll-core/core/src/head_digest.rs");
    let auth = include_str!("../../runtime/src/authorization.rs");
    let version = include_str!("../src/daemon/version.rs");

    // (code that declares it, the string the docs must carry)
    let constructions = [
        (
            parse,
            "const SEGMENT_ROOT_SCHEME: &str = \"leyline/segment-root/v1\";",
            "leyline/segment-root/v1",
        ),
        (
            store,
            "pub const STORE_TREE_SCHEME: &str = \"leyline/store-tree/v1\";",
            "leyline/store-tree/v1",
        ),
        (
            store,
            "pub const PARSER_ID_SCHEME: &str = \"leyline/parser-id/v1\";",
            "leyline/parser-id/v1",
        ),
        (
            digest,
            "pub const HEAD_DIGEST_CONTEXT: &str = \"leyline head digest v3\";",
            "leyline head digest v3",
        ),
        (
            parse,
            "const NODE_HASH_CONTEXT: &str = \"leyline/ast-node/v3\";",
            "leyline/ast-node/v3",
        ),
        (
            auth,
            "const RUN_ID_DOMAIN: &[u8] = b\"cloister/execution/v1/run-id\";",
            "cloister/execution/v1/run-id",
        ),
        (
            version,
            "pub const PROJECTION_SCHEMA_VERSION: &str = \"projection-v13\";",
            "projection-v13",
        ),
    ];
    for (code, declaration, name) in constructions {
        assert!(
            code.contains(declaration),
            "the code no longer declares `{declaration}`; update AGENTS.md, the review kit and this test"
        );
        assert!(
            agents.contains(name),
            "AGENTS.md's identity vocabulary does not name `{name}`"
        );
        assert!(
            kit.contains(name),
            "leyline-review-kit's authority ledger does not name `{name}`"
        );
    }

    // AGENTS.md names the partition fold every keyed scheme above folds under.
    let partition = include_str!("../../../ll-core/core/src/partition.rs");
    assert!(
        partition.contains("pub const PARTITION_CONTEXT: &str = \"leyline partition fold v1\";"),
        "partition.rs changed PARTITION_CONTEXT; update AGENTS.md and this test"
    );
    assert!(
        agents.contains("`BLAKE3_derive_key(\"leyline partition fold v1\", …)`"),
        "AGENTS.md must name the partition fold's context"
    );

    // run_id v1 is what ships: its row says so, apart from the v2 row.
    assert!(
        agents.contains("| Shipped (v1, prefix-in-preimage"),
        "AGENTS.md must list run_id v1 as the shipped construction"
    );

    // run_id v2 is ADR-0047 M3: not in the code, so proposed in both docs.
    assert!(
        !auth.contains("run-id/v2"),
        "authorization.rs derives run-id v2; mark it shipped in AGENTS.md and the review kit"
    );
    assert!(
        agents.contains(
            "| `run_id` v2 | The same run | `derive_key(\"cloister/execution/v1/run-id/v2\", …)`, rendered `run-v2-<hex>` | Proposed only"
        ),
        "AGENTS.md must list run_id v2 as proposed"
    );
    assert!(
        kit.contains("keyed v2 is ADR-0047 M3, proposed"),
        "the review kit must list run_id v2 as proposed"
    );

    // A locator is not an identity, and the docs say so.
    assert!(
        agents.contains("a locator, not an identity"),
        "AGENTS.md must say content_id and the other locators are not identities"
    );
}
