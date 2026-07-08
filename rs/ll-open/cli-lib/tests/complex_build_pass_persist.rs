//! Sheaf-invalidation Gap 2 regression pin (bead `ley-line-open-3af437`).
//!
//! Before this bead, `ComplexBuildPass::run` built a `CellComplex`
//! from `observation` rows, called `build_delta_0` as a mechanical-
//! reach witness, then dropped the complex when the function
//! returned (`docs/audits/sheaf-invalidation-trace.md` §Gap 2). Every
//! consumer `op_sheaf_*` query that needed the derived complex paid
//! to rebuild it.
//!
//! This test proves the persistence side channel: after `run` the
//! shared [`SheafState`] must hold the built complex with the
//! expected node + edge counts, and the co-change tracker must
//! reflect the observed events (surfaced via `op_sheaf_status`'s
//! `tracked_edges` field).
//!
//! Falsifiability: the assertions below FAIL if a future refactor
//! either drops the `install_complex` call from the end of `run` or
//! swaps to a shape that doesn't propagate node / edge counts (e.g.
//! installing an empty CellComplex from a spy path).

use std::sync::Arc;

use leyline_cli_lib::daemon::complex_build_pass::ComplexBuildPass;
use leyline_cli_lib::daemon::enrichment::EnrichmentPass;
use leyline_cli_lib::daemon::observation_schema::create_observation_schema;
use leyline_cli_lib::daemon::sheaf_ops::{SheafState, op_sheaf_status};
use rusqlite::Connection;

/// Seed 5 observations against 4 distinct tokens — matches the shape
/// used by the falsifiability gate in `complex_build_pass_gate.rs` so
/// the expected counts stay easy to reason about (4 unique tokens →
/// 4 nodes; 4 unique unordered pairs → 4 edges).
fn seed_observations(conn: &Connection) {
    let rows: [&str; 5] = [
        r#"["alpha","beta"]"#,
        r#"["alpha","gamma"]"#,
        r#"["beta","gamma"]"#,
        r#"["delta"]"#,
        r#"["alpha","delta"]"#,
    ];
    for (i, mentions) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO observation (source, payload_kind, mentions, observed_at) \
             VALUES ('test', 'agent.session_turn', ?1, ?2)",
            rusqlite::params![mentions, (i as i64) * 1000],
        )
        .unwrap();
    }
}

#[test]
fn complex_build_pass_installs_complex_into_sheaf_state() {
    // Fresh SheafState — no topology pushed via sheaf_set_topology, so
    // `cache.complex()` is `None` before `run`. If ComplexBuildPass
    // ever drops the complex on return again, this test fails at the
    // "must have installed a complex" assertion below.
    let sheaf = Arc::new(SheafState::new());

    let conn = Connection::open_in_memory().unwrap();
    create_observation_schema(&conn).unwrap();
    seed_observations(&conn);

    // Pre-condition: cache carries no complex before the pass runs.
    {
        let cache = sheaf.cache().lock().unwrap();
        assert!(
            cache.complex().is_none(),
            "pre-condition: SheafState cache must start with no complex",
        );
    }

    let pass = ComplexBuildPass::new(sheaf.clone());
    let stats = pass
        .run(&conn, std::path::Path::new("/"), None)
        .expect("ComplexBuildPass::run");

    // Sanity: pass reported per-observation work — 5 observations
    // processed, and the (nodes + edges) sum surfaces via items_added.
    // 4 unique tokens + 4 unique co-occurrence pairs = 8 items.
    assert_eq!(
        stats.pass_name, "complex-build",
        "pass_name pin — drift breaks the enrichment dispatch",
    );
    assert_eq!(
        stats.files_processed, 5,
        "expected 5 observations processed, got {stats:?}",
    );
    assert_eq!(
        stats.items_added, 8,
        "expected 4 nodes + 4 edges = 8 items_added, got {stats:?}",
    );

    // Post-condition: the built complex is now installed in the
    // shared cache with the expected node + edge counts. This is the
    // load-bearing assertion for bead `ley-line-open-3af437` — before
    // the fix, the cache's `complex()` accessor stayed `None` after
    // `run` because the pass dropped the built complex on return.
    {
        let cache = sheaf.cache().lock().unwrap();
        let cx = cache
            .complex()
            .expect("ComplexBuildPass must install a CellComplex into SheafState");
        assert_eq!(
            cx.nodes.len(),
            4,
            "installed complex must have 4 nodes (one per unique token); got {}",
            cx.nodes.len(),
        );
        assert_eq!(
            cx.edges.len(),
            4,
            "installed complex must have 4 co-occurrence edges; got {}",
            cx.edges.len(),
        );
    }

    // Post-condition: the CoChangeTracker was installed too. Every
    // observation drives one `tracker.observe(&changed, &all_edges)`
    // call with `all_edges` set to the full 4-edge co-occurrence
    // list, so the tracker's `edge_count` (== unique edge keys) must
    // be 4. Surfaced via `op_sheaf_status`.
    let status_json = op_sheaf_status(&sheaf).expect("op_sheaf_status");
    let status: serde_json::Value =
        serde_json::from_str(&status_json).expect("status response must be valid JSON");
    assert_eq!(
        status["tracked_edges"], 4,
        "installed CoChangeTracker must have 4 tracked edges; \
         drift here proves install_complex dropped the tracker on \
         the floor. Got status={status}",
    );

    // Post-condition: the region label map was installed too. Sheaf
    // gap 3 follow-up (bead `ley-line-open-e40566`): with labels
    // installed, the watcher-driven `daemon.sheaf.invalidate` emit
    // computes a fine-grained diff from `changed_files` instead of
    // falling back to `scope: "all-known"`. Falsifiability: probe
    // `regions_touching_files` — must return `Some(_)` because
    // labels are installed. The exact vec depends on how the token
    // fixture ("alpha", "beta", ...) is treated as a "file path",
    // which it isn't — bare-token labels don't match any path
    // shape, so `regions_touching_files(&["alpha".into()])` returns
    // `Some(vec![])` (not `None`). The returned `Some` — not `None`
    // — is the load-bearing pin: it distinguishes "labels installed
    // and diff computed" from "no labels, falling back to
    // all-known".
    let touched = sheaf.regions_touching_files(&["alpha".to_string()]);
    assert!(
        touched.is_some(),
        "regions_touching_files must return Some(_) once labels are \
         installed; None means ComplexBuildPass dropped the label \
         install_region_labels call and the watcher-driven \
         invalidate falls back to the coarse-v1 `all-known` scope"
    );
}

#[test]
fn complex_build_pass_default_ctor_does_not_touch_sheaf_state() {
    // Negative control: the `Default` constructor produces a pass
    // with `sheaf: None`, so `run` executes the same build side
    // effects but SKIPS the install. Existing test fixtures (and the
    // ADR-0020 Gate 2 falsifiability test in
    // `complex_build_pass_gate.rs`, which drives `build_complex`
    // directly) rely on this shape — a refactor that made the sheaf
    // install unconditional would break them silently.
    let sheaf = Arc::new(SheafState::new());

    let conn = Connection::open_in_memory().unwrap();
    create_observation_schema(&conn).unwrap();
    seed_observations(&conn);

    let pass = ComplexBuildPass::default();
    pass.run(&conn, std::path::Path::new("/"), None)
        .expect("ComplexBuildPass::run must succeed with no sheaf state");

    let cache = sheaf.cache().lock().unwrap();
    assert!(
        cache.complex().is_none(),
        "Default-constructed ComplexBuildPass must NOT touch the \
         shared SheafState — otherwise its `sheaf: None` invariant \
         is a lie and tests can't isolate the build path from the \
         install path.",
    );
}
