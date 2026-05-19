//! Default no-backend engine — every op returns [`Error::NotImplemented`].
//!
//! Purpose: lets the daemon op surface compile and respond with a structured
//! error before any real engine is wired in. Also serves as the canonical
//! unhappy-path target for the trait's contract tests.

use std::path::Path;

use crate::{Error, Hit, Result, TextSearchEngine};

#[derive(Debug, Default)]
pub struct NullEngine;

impl NullEngine {
    pub fn new() -> Self {
        Self
    }
}

impl TextSearchEngine for NullEngine {
    fn upsert(&self, _node_id: &str, _content: &str) -> Result<()> {
        Err(Error::NotImplemented("upsert"))
    }

    fn remove(&self, _node_id: &str) -> Result<()> {
        Err(Error::NotImplemented("remove"))
    }

    fn finalize(&self) -> Result<()> {
        Err(Error::NotImplemented("finalize"))
    }

    fn search(&self, _query: &str, _k: usize) -> Result<Vec<Hit>> {
        Err(Error::NotImplemented("search"))
    }

    fn len(&self) -> Result<usize> {
        Err(Error::NotImplemented("len"))
    }

    fn clear(&self) -> Result<()> {
        Err(Error::NotImplemented("clear"))
    }

    fn storage_path(&self) -> Option<&Path> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_not_implemented<T: std::fmt::Debug>(r: Result<T>, op: &str) {
        match r {
            Err(Error::NotImplemented(actual)) => assert_eq!(
                actual, op,
                "NotImplemented variant must carry the op name verbatim",
            ),
            other => panic!("expected NotImplemented({op:?}), got {other:?}"),
        }
    }

    #[test]
    fn every_op_returns_not_implemented_with_op_name() {
        // Pin: NullEngine's whole contract is "structured not-implemented".
        // If a future variant becomes Ok(...) silently — e.g. someone wires
        // an in-memory fallback for `len` — the daemon would start returning
        // confusing partial responses. This catches that drift on every op.
        let e = NullEngine::new();
        assert_not_implemented(e.upsert("n", "c"), "upsert");
        assert_not_implemented(e.remove("n"), "remove");
        assert_not_implemented(e.finalize(), "finalize");
        assert_not_implemented(e.search("q", 10), "search");
        assert_not_implemented(e.len(), "len");
        assert_not_implemented(e.clear(), "clear");
    }

    #[test]
    fn is_empty_propagates_len_error() {
        // The default `is_empty` impl delegates to `len`. The Null engine
        // therefore must surface NotImplemented for `is_empty` too — not
        // a phantom `Ok(true)`.
        let e = NullEngine::new();
        assert_not_implemented(e.is_empty(), "len");
    }

    #[test]
    fn storage_path_is_none() {
        // The substrate-non-leak gate keys off storage_path(). For an
        // engine that has no storage at all, None is the unambiguous
        // answer — the gate treats "no path" as trivially non-leaking.
        let e = NullEngine::new();
        assert!(e.storage_path().is_none());
    }

    #[test]
    fn error_display_includes_op_name() {
        // Error::NotImplemented Display must surface the op name so the
        // daemon's MCP error response is actionable ("which op was it?").
        let err = Error::NotImplemented("upsert");
        let s = err.to_string();
        assert!(
            s.contains("upsert"),
            "Display must include op name; got: {s}"
        );
    }
}
