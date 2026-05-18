//! Witchcraft engine stub.
//!
//! # Why this is a stub today
//!
//! The Witchcraft crate (<https://github.com/dropbox/witchcraft>) pins
//! `rusqlite = ^0.39.0` (via `libsqlite3-sys ^0.37.0`). This workspace
//! pins `rusqlite = 0.34.0` (via `libsqlite3-sys 0.32.0`) across at
//! minimum:
//!
//! - `leyline-cli-lib` (the daemon's living db)
//! - `leyline-chat-embed`
//! - `leyline-fs` (transitively, via `cli-lib`)
//!
//! `libsqlite3-sys` declares `links = "sqlite3"`, so Cargo's link-key rule
//! permits exactly one version per dependency graph. Adding Witchcraft as
//! a workspace member crate today breaks resolution **for the entire
//! workspace**, not just for the consumer that activates the feature —
//! `cargo check -p leyline-cli` fails identically.
//!
//! Reproduction:
//!
//! ```text
//! $ cargo check -p leyline-text-search --features engine-witchcraft
//! error: failed to select a version for `libsqlite3-sys`.
//!     ... required by package `rusqlite v0.39.0`
//!     ... which satisfies git dependency `witchcraft` ...
//!     ... conflicts with `libsqlite3-sys v0.32.0`
//!     ... required by `rusqlite v0.34.0`
//! ```
//!
//! # The three unblock paths
//!
//! 1. **Upgrade workspace rusqlite to ≥0.39.** Touches every crate that
//!    constructs a `Connection`. Cleanest end state; biggest blast radius.
//!    `cli-lib`'s `sqlite3_deserialize` zero-copy arena path needs an audit
//!    against the 0.34→0.39 changelog before this is safe.
//!
//! 2. **Vendor a patched Witchcraft** that targets rusqlite 0.34. Smallest
//!    diff to ley-line; fragile (upstream drifts), and Witchcraft's
//!    candle/tokenizers stack may have transitive constraints that pin a
//!    newer rusqlite anyway.
//!
//! 3. **Out-of-process Witchcraft** — shell out to `warp-cli` over a UDS or
//!    pipe. No shared linker; full isolation from the workspace's deps.
//!    Adds IPC overhead and a runtime binary dependency, but lets the rest
//!    of the integration (trait, NullEngine, daemon op, eval gates) ship
//!    today.
//!
//! Until one of those lands, this module exposes [`WitchcraftStub`] — a
//! `TextSearchEngine` impl that returns [`Error::NotImplemented`] for
//! every op, with the op name and a pointer back to this docstring in the
//! error message so a misconfigured deployment surfaces the situation at
//! the first call, not at debug time.

use std::path::Path;

use crate::{Error, Hit, Result, TextSearchEngine};

/// Placeholder for the future real `WitchcraftEngine`. Returns
/// [`Error::NotImplemented`] for every op — see this module's docstring
/// for the rusqlite-skew blocker and unblock paths.
#[derive(Debug, Default)]
pub struct WitchcraftStub;

impl WitchcraftStub {
    pub fn new() -> Self {
        Self
    }
}

impl TextSearchEngine for WitchcraftStub {
    fn upsert(&self, _node_id: &str, _content: &str) -> Result<()> {
        Err(Error::NotImplemented(
            "witchcraft.upsert (blocked by rusqlite skew; see leyline_text_search::witchcraft docs)",
        ))
    }

    fn remove(&self, _node_id: &str) -> Result<()> {
        Err(Error::NotImplemented(
            "witchcraft.remove (blocked by rusqlite skew; see leyline_text_search::witchcraft docs)",
        ))
    }

    fn finalize(&self) -> Result<()> {
        Err(Error::NotImplemented(
            "witchcraft.finalize (blocked by rusqlite skew; see leyline_text_search::witchcraft docs)",
        ))
    }

    fn search(&self, _query: &str, _k: usize) -> Result<Vec<Hit>> {
        Err(Error::NotImplemented(
            "witchcraft.search (blocked by rusqlite skew; see leyline_text_search::witchcraft docs)",
        ))
    }

    fn len(&self) -> Result<usize> {
        Err(Error::NotImplemented(
            "witchcraft.len (blocked by rusqlite skew; see leyline_text_search::witchcraft docs)",
        ))
    }

    fn clear(&self) -> Result<()> {
        Err(Error::NotImplemented(
            "witchcraft.clear (blocked by rusqlite skew; see leyline_text_search::witchcraft docs)",
        ))
    }

    fn storage_path(&self) -> Option<&Path> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin: every WitchcraftStub op returns NotImplemented with a message
    /// that names the op AND points the reader at the unblock docs. If
    /// someone wires a partial real impl behind this stub without
    /// updating the message, the misdirection is caught here.
    #[test]
    fn every_op_returns_not_implemented_with_blocker_pointer() {
        let e = WitchcraftStub::new();
        for (label, err) in [
            ("upsert", e.upsert("n", "c").unwrap_err()),
            ("remove", e.remove("n").unwrap_err()),
            ("finalize", e.finalize().unwrap_err()),
            ("search", e.search("q", 10).map(|_| ()).unwrap_err()),
            ("len", e.len().map(|_| ()).unwrap_err()),
            ("clear", e.clear().unwrap_err()),
        ] {
            match err {
                Error::NotImplemented(msg) => {
                    assert!(
                        msg.starts_with(&format!("witchcraft.{label}")),
                        "op {label}: message must start with `witchcraft.{label}`; got: {msg}",
                    );
                    assert!(
                        msg.contains("rusqlite skew"),
                        "op {label}: message must cite the rusqlite-skew blocker so operators \
                         see why the engine isn't live; got: {msg}",
                    );
                    assert!(
                        msg.contains("leyline_text_search::witchcraft"),
                        "op {label}: message must point at the docs module so the reader can \
                         find the unblock paths; got: {msg}",
                    );
                }
                other => panic!("op {label}: expected NotImplemented, got {other:?}"),
            }
        }
    }

    #[test]
    fn storage_path_is_none_for_stub() {
        // The stub has no storage. The substrate non-leak gate treats
        // None as trivially-non-leaking. When the real impl lands and
        // returns Some(path), the gate must assert that path is OUTSIDE
        // the arena directory.
        assert!(WitchcraftStub::new().storage_path().is_none());
    }
}
