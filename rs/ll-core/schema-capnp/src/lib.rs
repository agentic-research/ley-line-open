//! Cap'n Proto schemas for the Σ event log.
//!
//! T8 (decade ley-line-open-9d30ac, thread T8/capnp-as-protocol).
//!
//! Each `schemas/*.capnp` file becomes a generated module re-exported
//! here. Consumers depend on this crate; producers (cmd_parse,
//! project_references, etc.) write capnp messages whose Rust types are
//! generated from these schemas.
//!
//! # Schema evolution
//!
//! See `docs/adr/0014-capnp-as-protocol.md` (T8.6). In brief: append
//! fields at next ordinal; never rename or repurpose; never remove —
//! leave a hole.
//!
//! # Why a separate crate
//!
//! The `leyline-schema` crate holds SQLite DDL — local-projection
//! schemas. This crate holds *contract* schemas: what crosses the
//! producer/consumer boundary. SQLite tables are derived from the
//! capnp event log per the Σ data-plane reframe (2026-05-08, L2/L3
//! synthesis).

#![allow(clippy::all)]

pub mod canonical;

pub mod common_capnp {
    include!(concat!(env!("OUT_DIR"), "/common_capnp.rs"));
}

pub mod binding_capnp {
    include!(concat!(env!("OUT_DIR"), "/binding_capnp.rs"));
}

pub mod ast_capnp {
    include!(concat!(env!("OUT_DIR"), "/ast_capnp.rs"));
}

pub mod source_capnp {
    include!(concat!(env!("OUT_DIR"), "/source_capnp.rs"));
}

pub mod head_capnp {
    include!(concat!(env!("OUT_DIR"), "/head_capnp.rs"));
}

pub mod cache_capnp {
    include!(concat!(env!("OUT_DIR"), "/cache_capnp.rs"));
}

#[cfg(test)]
mod tests {
    use super::cache_capnp::{
        cache_lockfile, meta, processor_version, source_entry, topology_edge,
    };
    use super::common_capnp::{hash, position, range};
    use capnp::message::{Builder, ReaderOptions};
    use capnp::serialize;

    /// T8.1 smoke test: build a Position, serialize, deserialize,
    /// fields round-trip. Pins the codegen wiring — if this fails,
    /// build.rs / capnpc didn't run, OR the schema's ordinals drifted.
    #[test]
    fn position_round_trips() {
        let mut msg = Builder::new_default();
        {
            let mut pos: position::Builder = msg.init_root();
            pos.set_line(42);
            pos.set_column(7);
            pos.set_byte(1234);
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let pos: position::Reader = reader.get_root().unwrap();

        assert_eq!(pos.get_line(), 42);
        assert_eq!(pos.get_column(), 7);
        assert_eq!(pos.get_byte(), 1234);
    }

    /// Pin the nested-struct shape: a Range is two Positions.
    /// If the schema's `start`/`end` ordinals ever drift, this fails
    /// at the field-name level (compile error) before any data
    /// corruption can happen on disk.
    #[test]
    fn range_nested_positions_round_trip() {
        let mut msg = Builder::new_default();
        {
            let mut r: range::Builder = msg.init_root();
            {
                let mut s = r.reborrow().init_start();
                s.set_line(1);
                s.set_column(2);
                s.set_byte(10);
            }
            {
                let mut e = r.reborrow().init_end();
                e.set_line(3);
                e.set_column(4);
                e.set_byte(20);
            }
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let r: range::Reader = reader.get_root().unwrap();

        let s = r.get_start().unwrap();
        let e = r.get_end().unwrap();
        assert_eq!(s.get_line(), 1);
        assert_eq!(s.get_byte(), 10);
        assert_eq!(e.get_column(), 4);
        assert_eq!(e.get_byte(), 20);
    }

    // ── cache.capnp smoke tests (ADR-0021 / ley-line-open-ae89aa) ──────
    //
    // These pin the cache schema's wire shape end-to-end. Failures here
    // mean either build.rs didn't run, ordinals drifted, or the import
    // of common.Hash got broken. The full TOML↔capnp + capnp↔OCI-JSON
    // round-trip suite lives in tests/cache_roundtrip.rs.

    /// Helper: construct a 32-byte BLAKE3-shaped Hash via the builder.
    fn write_hash(mut h: hash::Builder, bytes: &[u8; 32]) {
        h.set_bytes(bytes);
    }

    /// Pin the SourceEntry shape: path + two hashes + kind. If any
    /// field-name or ordinal drifts, this fails at compile time.
    #[test]
    fn source_entry_round_trips() {
        let input_bytes = [0x11u8; 32];
        let chunk_bytes = [0x22u8; 32];

        let mut msg = Builder::new_default();
        {
            let mut se: source_entry::Builder = msg.init_root();
            se.set_path("src/auth.go");
            write_hash(se.reborrow().init_input_hash(), &input_bytes);
            write_hash(se.reborrow().init_chunk_hash(), &chunk_bytes);
            se.set_kind("go-source");
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let se: source_entry::Reader = reader.get_root().unwrap();

        assert_eq!(se.get_path().unwrap().to_str().unwrap(), "src/auth.go");
        assert_eq!(se.get_kind().unwrap().to_str().unwrap(), "go-source");
        assert_eq!(
            se.get_input_hash().unwrap().get_bytes().unwrap(),
            &input_bytes
        );
        assert_eq!(
            se.get_chunk_hash().unwrap().get_bytes().unwrap(),
            &chunk_bytes
        );
    }

    /// Pin the Meta shape including the inputProcessors list. Catches
    /// ordinal drift on producer/version/schemaVersion AND on the
    /// nested ProcessorVersion list.
    #[test]
    fn meta_with_processors_round_trips() {
        let mut msg = Builder::new_default();
        {
            let mut m: meta::Builder = msg.init_root();
            m.set_producer("mache");
            m.set_producer_version("0.7.1");
            m.set_schema_version("0.1.0");
            m.set_generated_at_ms(1_748_345_600_000);
            let mut procs = m.init_input_processors(2);
            {
                let mut p = procs.reborrow().get(0);
                p.set_kind("tree-sitter-go");
                p.set_version("0.21.0");
            }
            {
                let mut p = procs.reborrow().get(1);
                p.set_kind("blake3");
                p.set_version("1.5.0");
            }
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let m: meta::Reader = reader.get_root().unwrap();

        assert_eq!(m.get_producer().unwrap().to_str().unwrap(), "mache");
        assert_eq!(m.get_producer_version().unwrap().to_str().unwrap(), "0.7.1");
        assert_eq!(m.get_schema_version().unwrap().to_str().unwrap(), "0.1.0");
        assert_eq!(m.get_generated_at_ms(), 1_748_345_600_000);

        let procs = m.get_input_processors().unwrap();
        assert_eq!(procs.len(), 2);
        assert_eq!(
            procs.get(0).get_kind().unwrap().to_str().unwrap(),
            "tree-sitter-go"
        );
        assert_eq!(
            procs.get(0).get_version().unwrap().to_str().unwrap(),
            "0.21.0"
        );
        assert_eq!(procs.get(1).get_kind().unwrap().to_str().unwrap(), "blake3");
        assert_eq!(
            procs.get(1).get_version().unwrap().to_str().unwrap(),
            "1.5.0"
        );
    }

    /// Pin the full CacheLockfile assembly: meta + N sources + N edges +
    /// root. If anything drifts at the top level, this catches it.
    #[test]
    fn cache_lockfile_full_round_trip() {
        let root_bytes = [0xFFu8; 32];

        let mut msg = Builder::new_default();
        {
            let mut lf: cache_lockfile::Builder = msg.init_root();
            {
                let mut m = lf.reborrow().init_meta();
                m.set_producer("mache");
                m.set_producer_version("0.7.1");
                m.set_schema_version("0.1.0");
                m.set_generated_at_ms(1_748_345_600_000);
                let mut procs = m.init_input_processors(1);
                let mut p = procs.reborrow().get(0);
                p.set_kind("tree-sitter-go");
                p.set_version("0.21.0");
            }
            {
                let mut sources = lf.reborrow().init_sources(2);
                {
                    let mut s = sources.reborrow().get(0);
                    s.set_path("src/main.go");
                    write_hash(s.reborrow().init_input_hash(), &[0x01u8; 32]);
                    write_hash(s.reborrow().init_chunk_hash(), &[0x10u8; 32]);
                    s.set_kind("go-source");
                }
                {
                    let mut s = sources.reborrow().get(1);
                    s.set_path("src/auth.go");
                    write_hash(s.reborrow().init_input_hash(), &[0x02u8; 32]);
                    write_hash(s.reborrow().init_chunk_hash(), &[0x20u8; 32]);
                    s.set_kind("go-source");
                }
            }
            {
                let mut edges = lf.reborrow().init_topology(1);
                let mut e = edges.reborrow().get(0);
                e.set_from("src/main.go");
                e.set_to_source("src/auth.go");
            }
            write_hash(lf.reborrow().init_root(), &root_bytes);
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let lf: cache_lockfile::Reader = reader.get_root().unwrap();

        assert_eq!(
            lf.get_meta()
                .unwrap()
                .get_producer()
                .unwrap()
                .to_str()
                .unwrap(),
            "mache"
        );
        assert_eq!(lf.get_sources().unwrap().len(), 2);
        assert_eq!(lf.get_topology().unwrap().len(), 1);
        assert_eq!(
            lf.get_topology()
                .unwrap()
                .get(0)
                .get_from()
                .unwrap()
                .to_str()
                .unwrap(),
            "src/main.go"
        );
        assert_eq!(lf.get_root().unwrap().get_bytes().unwrap(), &root_bytes);
    }

    /// Empty lockfile is valid: no sources, no topology, default root.
    /// This pins the "first push, no chunks yet" / "scope is empty repo"
    /// edge case.
    #[test]
    fn empty_cache_lockfile_round_trips() {
        let mut msg = Builder::new_default();
        {
            let mut lf: cache_lockfile::Builder = msg.init_root();
            let mut m = lf.reborrow().init_meta();
            m.set_producer("mache");
            m.set_schema_version("0.1.0");
            m.set_generated_at_ms(0);
            // Default-initialize sources/topology/root by not touching them.
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let lf: cache_lockfile::Reader = reader.get_root().unwrap();

        assert_eq!(lf.get_sources().unwrap().len(), 0);
        assert_eq!(lf.get_topology().unwrap().len(), 0);
        // Default Hash has zero-length Data — useful sentinel for
        // "this lockfile hasn't been finalized."
        assert_eq!(lf.get_root().unwrap().get_bytes().unwrap().len(), 0);
    }

    /// Pin that ProcessorVersion is independently constructible (it gets
    /// used both inline in Meta.inputProcessors and potentially as a
    /// standalone for cross-lockfile compatibility checks).
    #[test]
    fn processor_version_round_trips() {
        let mut msg = Builder::new_default();
        {
            let mut pv: processor_version::Builder = msg.init_root();
            pv.set_kind("tree-sitter-rust");
            pv.set_version("0.21.0");
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let pv: processor_version::Reader = reader.get_root().unwrap();

        assert_eq!(pv.get_kind().unwrap().to_str().unwrap(), "tree-sitter-rust");
        assert_eq!(pv.get_version().unwrap().to_str().unwrap(), "0.21.0");
    }

    /// Pin that TopologyEdge is independently constructible. Same
    /// rationale as processor_version_round_trips.
    #[test]
    fn topology_edge_round_trips() {
        let mut msg = Builder::new_default();
        {
            let mut e: topology_edge::Builder = msg.init_root();
            e.set_from("src/lib.rs");
            e.set_to_source("src/internal/auth.rs");
        }

        let mut buf = Vec::new();
        serialize::write_message(&mut buf, &msg).unwrap();

        let reader = serialize::read_message(buf.as_slice(), ReaderOptions::new()).unwrap();
        let e: topology_edge::Reader = reader.get_root().unwrap();

        assert_eq!(e.get_from().unwrap().to_str().unwrap(), "src/lib.rs");
        assert_eq!(
            e.get_to_source().unwrap().to_str().unwrap(),
            "src/internal/auth.rs"
        );
    }
}
