//! The live sheaf region cache against the restriction key, on the same git
//! replay (beads `ley-line-open-754aa1`, `ley-line-open-f04610`).
//!
//! `leyline-cache`'s `restriction_git_replay` measures three key-strategy
//! arms. This bench runs the SAME history through the SAME driver
//! (`leyline-cache`'s `tests/common`, included by path) and adds two arms
//! backed by [`SheafCache`], the region cache the daemon serves over
//! `sheaf_set_topology` / `sheaf_invalidate` (`cli-lib/src/daemon/sheaf_ops.rs`):
//!
//! | arm | invalidation |
//! |---|---|
//! | `sheaf-heuristic` | XOR-of-Merkle-roots boundary check, BFS bounded at `HEURISTIC_CASCADE_DEPTH` (3) |
//! | `sheaf-delta0` | the same pre-filter, confirmed by per-edge ‖δ⁰‖ movement on an attached `CellComplex` |
//!
//! It lives in `leyline-sheaf` because it needs both crates: `leyline-sheaf`
//! dev-depends on `leyline-cache`, and `leyline-cache` depends on nothing in
//! `leyline-sheaf`, so there is no cycle.
//!
//! The adapter, including the δ⁰ stalk's agreement-subspace layout (interface
//! digest in the kept coordinates, body digest in the projected-away ones), is
//! `tests/sheaf_arm/mod.rs`, shared with the `sheaf_cache_advisory`
//! regression test.
//!
//! The SheafCache arms do NOT claim soundness: `SheafCache` is advisory (see
//! its module doc). Its region graph holds only the parent state's aligned
//! functions, so it cannot see a dependency that the value reads but no
//! region owns — a def of a called token appearing or disappearing elsewhere,
//! or an import edit outside the function's bytes. Their false skips are
//! reported, not asserted.
//!
//! ```text
//! task cache:ablation
//! # or: cargo bench -p leyline-sheaf --bench sheaf_restriction_replay
//! ```

#[path = "../../cache/tests/common/mod.rs"]
mod common;
#[path = "../tests/sheaf_arm/mod.rs"]
mod sheaf_arm;

use common::ablation::{Arm, KeyArm};
use leyline_cache::strategy::{NoCache, RestrictionKey, WholeInput};
use sheaf_arm::SheafArm;

fn main() {
    let mut arms: Vec<Box<dyn Arm>> = vec![
        Box::new(KeyArm::new(NoCache)),
        Box::new(KeyArm::new(WholeInput)),
        Box::new(KeyArm::new(RestrictionKey)),
        Box::new(SheafArm::new(false)),
        Box::new(SheafArm::new(true)),
    ];
    common::git::replay_main(
        "SheafCache vs restriction key: call-target review, git replay",
        env!("CARGO_MANIFEST_DIR"),
        &mut arms,
    );
}
