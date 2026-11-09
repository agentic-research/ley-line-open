//! Git-replay ablation of the restriction-addressed review cache (bead
//! `ley-line-open-754aa1`; superset-correctness replay of ADR-0031 caveat #3,
//! bead `ley-line-open-055f79`).
//!
//! # The claim this stresses
//!
//! ADR-0031: for the call-target review of a function `F`,
//! **restriction-unchanged ⇒ review-output-unchanged**. A restriction key that
//! is unchanged while the review changed is a FALSE SKIP: the closure missed
//! a dependency. The replay hunts for one over real edits from this repo's
//! history, and compares the restriction key against the two arms it must be
//! measured against.
//!
//! # Arms (one `ReviewCache` interface, three `KeyStrategy`s)
//!
//! | arm | key |
//! |---|---|
//! | `no-cache` | none: every lookup recomputes |
//! | `whole-input` | `whole_input_key(storeRoot, parserId, target)` |
//! | `restriction` | `restriction_for_call_target`: the closure-row fold |
//!
//! The store key here is `whole_object_hash` of the step's corpus, a flat
//! `RowSet` fold over `(path, σ(bytes))`: a different construction from
//! `Head.storeRoot` (no modes, no directories), standing in for it as the
//! tightest whole-input key, so the comparison favours the whole-input arm.
//!
//! # Method
//!
//! Replay the N most recent non-merge commits touching `rs/`, oldest first
//! (`tests/common/git.rs`). For each commit the CORPUS is the set of `.rs`
//! files it changed that exist on both the parent and the commit, so the
//! review resolves call targets across co-changed files. Each step warms
//! every arm on the parent's values, then asks it for every aligned function
//! of the commit (`tests/common/ablation.rs` defines the metrics). Caches
//! persist across steps. `leyline-sheaf`'s `sheaf_restriction_replay` bench
//! runs the same history with the `SheafCache` arms added.
//!
//! # Verdict
//!
//! Any false skip in an arm that claims soundness is printed with its exact
//! commit / file / function and `main` exits non-zero. Counts are
//! deterministic for a given history; timings are reported, not asserted.
//!
//! # How to run
//!
//! ```text
//! task cache:ablation
//! # or: cargo bench -p leyline-cache --bench restriction_git_replay
//! # knobs (env): RESTR_N_COMMITS (default 60), RESTR_MAX_CORPUS_FILES (default 40)
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use common::ablation::{Arm, KeyArm};
use leyline_cache::strategy::{NoCache, RestrictionKey, WholeInput};

fn main() {
    let mut arms: Vec<Box<dyn Arm>> = vec![
        Box::new(KeyArm::new(NoCache)),
        Box::new(KeyArm::new(WholeInput)),
        Box::new(KeyArm::new(RestrictionKey)),
    ];
    common::git::replay_main(
        "leyline-cache ablation: call-target review, git replay",
        env!("CARGO_MANIFEST_DIR"),
        &mut arms,
    );
}
