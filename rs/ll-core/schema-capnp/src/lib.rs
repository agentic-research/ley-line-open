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

#[cfg(test)]
mod tests {
    use super::common_capnp::{position, range};
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
}
