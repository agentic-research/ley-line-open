//! Combined-view hypervector: XOR-bind across all populated layers.
//!
//! Per math-friend review F. The combined view is an *unweighted
//! prefilter* — fast top-k pruning before a per-layer weighted
//! rerank. HDC has no clean weighted-bind operator (XOR is
//! self-inverse, not amplifying), so the multi-scale equivalence
//! query path is:
//!
//!   1. Compute combined-view HV for the query: XOR-bind per-layer
//!      HVs through their layer-tag role permutations.
//!   2. Top-K against `_hdc_combined` via popcount_xor (O(N)).
//!   3. For the top-K candidates, fetch per-layer HVs from `_hdc`
//!      and rerank with caller-supplied weights.
//!
//! This module provides step 1 (build the combined HV from a
//! per-layer map) and the persistence path (write to `_hdc_combined`).

use std::collections::HashMap;

use rusqlite::Connection;

use crate::util::{rotate_left, xor_into, Hypervector, ZERO_HV};
use crate::LayerKind;

/// Stable role-permutation index per layer. Layer-tagged so a
/// combined view can recover the per-layer contribution at unbind
/// time. Indices are permanent — bumping them orphans every encoded
/// combined hypervector.
fn layer_role_index(layer: LayerKind) -> usize {
    match layer {
        LayerKind::Ast => 0,
        LayerKind::Module => 1,
        LayerKind::Semantic => 2,
        LayerKind::Temporal => 3,
        LayerKind::Hir => 4,
        LayerKind::Lex => 5,
        LayerKind::Fs => 6,
    }
}

/// Build the combined-view hypervector from a per-layer map. Each
/// layer's HV is rotated by `layer_role_index(layer)` bits and
/// XOR-bundled into the accumulator. Layers absent from the map
/// contribute nothing — the combined view degrades gracefully when
/// some layers haven't been populated yet.
///
/// Deterministic: same `layers` input produces the same output on
/// every machine, every run.
pub fn build_combined_hv(layers: &HashMap<LayerKind, Hypervector>) -> Hypervector {
    let mut acc = ZERO_HV;
    // Iterate in deterministic layer-role-index order, not HashMap
    // iteration order, so the output is independent of insertion
    // order.
    for kind in [
        LayerKind::Ast,
        LayerKind::Module,
        LayerKind::Semantic,
        LayerKind::Temporal,
        LayerKind::Hir,
        LayerKind::Lex,
        LayerKind::Fs,
    ] {
        if let Some(hv) = layers.get(&kind) {
            let permuted = rotate_left(hv, layer_role_index(kind));
            xor_into(&mut acc, &permuted);
        }
    }
    acc
}

/// Build the combined view for a `scope_id` from rows in `_hdc`.
/// Fetches every layer's HV for that scope and XOR-binds them.
/// Returns the combined HV plus the max basis seen across layers
/// (used as the combined row's basis for cache invalidation).
pub fn build_combined_for_scope(
    conn: &Connection,
    scope_id: &str,
) -> rusqlite::Result<(Hypervector, i64)> {
    let mut layers = HashMap::new();
    let mut max_basis: i64 = 0;
    let mut stmt = conn.prepare_cached(
        "SELECT layer_kind, hv, basis FROM _hdc WHERE scope_id = ?1",
    )?;
    let rows = stmt.query_map([scope_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, i64>(2)?))
    })?;
    for row in rows {
        let (kind_str, blob, basis) = row?;
        if let Some(kind) = LayerKind::parse_str(&kind_str)
            && let Ok(hv) = blob.try_into() as std::result::Result<Hypervector, Vec<u8>>
        {
            layers.insert(kind, hv);
        }
        max_basis = max_basis.max(basis);
    }
    Ok((build_combined_hv(&layers), max_basis))
}

/// Refresh the combined view for one scope: build the HV, INSERT OR
/// REPLACE into `_hdc_combined`. Idempotent.
pub fn refresh_combined_for_scope(conn: &Connection, scope_id: &str) -> rusqlite::Result<()> {
    let (hv, basis) = build_combined_for_scope(conn, scope_id)?;
    conn.execute(
        "INSERT OR REPLACE INTO _hdc_combined(scope_id, hv, basis) VALUES (?1, ?2, ?3)",
        rusqlite::params![scope_id, hv.to_vec(), basis],
    )?;
    Ok(())
}

/// Refresh combined views for every distinct scope_id in `_hdc`.
/// Single-pass; useful at daemon startup or after a bulk reparse.
pub fn refresh_all_combined(conn: &Connection) -> rusqlite::Result<usize> {
    let scope_ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT scope_id FROM _hdc")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.filter_map(Result::ok).collect()
    };
    let count = scope_ids.len();
    for scope_id in scope_ids {
        refresh_combined_for_scope(conn, &scope_id)?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{conn_with_schema as fresh, insert_layer_hv as insert_layer};
    use crate::util::{assert_far_apart, expand_seed, popcount_distance};
    use crate::D_BYTES;

    #[test]
    fn empty_layer_map_returns_zero() {
        let layers = HashMap::new();
        assert_eq!(build_combined_hv(&layers), [0u8; D_BYTES]);
    }

    #[test]
    fn single_layer_returns_rotated_layer_hv() {
        let ast_hv = expand_seed(0x42);
        let layers = HashMap::from([(LayerKind::Ast, ast_hv)]);
        let combined = build_combined_hv(&layers);
        // Ast role-index = 0 → no rotation → combined = ast_hv.
        assert_eq!(combined, ast_hv);
    }

    #[test]
    fn module_layer_rotation_applied() {
        // Module role-index = 1, so the contribution is rotate_left(hv, 1).
        let mod_hv = expand_seed(0x99);
        let layers = HashMap::from([(LayerKind::Module, mod_hv)]);
        let combined = build_combined_hv(&layers);
        let expected = rotate_left(&mod_hv, 1);
        assert_eq!(combined, expected);
    }

    #[test]
    fn build_is_deterministic_regardless_of_insertion_order() {
        // Iteration order over the layer enum is fixed (Ast, Module,
        // Semantic, Temporal, Hir, Lex, Fs). HashMap iteration order
        // is NOT stable across rebuilds. Pin the property: result must
        // be independent of insertion order.
        let ast = expand_seed(1);
        let module = expand_seed(2);
        let semantic = expand_seed(3);

        let order_a = HashMap::from([
            (LayerKind::Ast, ast),
            (LayerKind::Module, module),
            (LayerKind::Semantic, semantic),
        ]);
        let order_b = HashMap::from([
            (LayerKind::Semantic, semantic),
            (LayerKind::Module, module),
            (LayerKind::Ast, ast),
        ]);

        assert_eq!(build_combined_hv(&order_a), build_combined_hv(&order_b));
    }

    #[test]
    fn distinct_layer_combinations_produce_distinct_hvs() {
        // {Ast: a} vs {Module: a} (same HV, different layer slot)
        // must produce different combined HVs because Module's
        // role-index = 1 rotates the bits.
        let hv = expand_seed(0xCAFE);
        let as_ast = HashMap::from([(LayerKind::Ast, hv)]);
        let as_module = HashMap::from([(LayerKind::Module, hv)]);
        assert_far_apart(
            &build_combined_hv(&as_ast),
            &build_combined_hv(&as_module),
            "same HV under different layers must produce far-apart combined views",
        );
    }

    #[test]
    fn add_layer_changes_combined_unless_zero() {
        // Adding a non-zero layer to an existing combined view must
        // change it. Adding a zero-HV layer is a no-op.
        let ast = expand_seed(0xAA);
        let module = expand_seed(0xBB);

        let just_ast = HashMap::from([(LayerKind::Ast, ast)]);
        let ast_and_module =
            HashMap::from([(LayerKind::Ast, ast), (LayerKind::Module, module)]);

        let combined_1 = build_combined_hv(&just_ast);
        let combined_2 = build_combined_hv(&ast_and_module);
        assert_ne!(combined_1, combined_2, "adding Module must change combined");

        // Zero-HV layer addition is a no-op.
        let ast_plus_zero =
            HashMap::from([(LayerKind::Ast, ast), (LayerKind::Semantic, ZERO_HV)]);
        assert_eq!(
            build_combined_hv(&ast_plus_zero),
            combined_1,
            "zero-HV layer addition must be a no-op",
        );
    }

    #[test]
    fn build_combined_for_scope_reads_from_hdc_table() {
        let conn = fresh();
        let ast = expand_seed(0x11);
        let module = expand_seed(0x22);

        insert_layer(&conn, "fn_foo", LayerKind::Ast, &ast, 1);
        insert_layer(&conn, "fn_foo", LayerKind::Module, &module, 2);

        let (combined, basis) = build_combined_for_scope(&conn, "fn_foo").unwrap();
        // Compare against in-memory build.
        let expected_layers =
            HashMap::from([(LayerKind::Ast, ast), (LayerKind::Module, module)]);
        assert_eq!(combined, build_combined_hv(&expected_layers));
        // Basis should be max(1, 2) = 2.
        assert_eq!(basis, 2);
    }

    #[test]
    fn build_combined_for_unknown_scope_returns_zero_hv() {
        // No rows in _hdc for that scope → empty layer map → zero HV,
        // basis 0. The radius calibration should treat zero-HV
        // combined entries as "uninitialized" rather than as a
        // legitimate match for any query.
        let conn = fresh();
        let (combined, basis) = build_combined_for_scope(&conn, "never_seen").unwrap();
        assert_eq!(combined, ZERO_HV);
        assert_eq!(basis, 0);
    }

    #[test]
    fn refresh_combined_inserts_or_replaces() {
        let conn = fresh();
        let ast_v1 = expand_seed(0xAA);
        insert_layer(&conn, "fn_foo", LayerKind::Ast, &ast_v1, 1);
        refresh_combined_for_scope(&conn, "fn_foo").unwrap();

        let row1: Vec<u8> = conn
            .query_row(
                "SELECT hv FROM _hdc_combined WHERE scope_id = ?1",
                ["fn_foo"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row1, ast_v1.to_vec());

        // Update _hdc with a new AST HV (different basis), refresh again.
        conn.execute(
            "UPDATE _hdc SET hv = ?1, basis = ?2 WHERE scope_id = ?3 AND layer_kind = ?4",
            rusqlite::params![expand_seed(0xBB).to_vec(), 5i64, "fn_foo", "ast"],
        )
        .unwrap();
        refresh_combined_for_scope(&conn, "fn_foo").unwrap();

        let row2: Vec<u8> = conn
            .query_row(
                "SELECT hv FROM _hdc_combined WHERE scope_id = ?1",
                ["fn_foo"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row2, expand_seed(0xBB).to_vec(), "refresh must overwrite");

        // basis updated too.
        let basis: i64 = conn
            .query_row(
                "SELECT basis FROM _hdc_combined WHERE scope_id = ?1",
                ["fn_foo"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(basis, 5);
    }

    #[test]
    fn refresh_all_combined_processes_every_distinct_scope() {
        let conn = fresh();
        for i in 0..5 {
            let scope = format!("fn_{i}");
            let hv = expand_seed(i);
            insert_layer(&conn, &scope, LayerKind::Ast, &hv, 1);
        }
        let count = refresh_all_combined(&conn).unwrap();
        assert_eq!(count, 5);
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM _hdc_combined", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 5);
    }

    #[test]
    fn combined_distance_smaller_when_more_layers_match() {
        // Two scopes that share AST + Module layers should produce
        // closer combined HVs than two scopes that share only AST.
        let conn = fresh();
        let ast_a = expand_seed(0xA1);
        let mod_a = expand_seed(0xA2);
        let ast_b = expand_seed(0xB1);
        let mod_b = expand_seed(0xB2);

        // Scope X: ast_a + mod_a
        insert_layer(&conn, "x", LayerKind::Ast, &ast_a, 1);
        insert_layer(&conn, "x", LayerKind::Module, &mod_a, 1);
        // Scope Y: ast_a + mod_a (clones at both layers)
        insert_layer(&conn, "y", LayerKind::Ast, &ast_a, 1);
        insert_layer(&conn, "y", LayerKind::Module, &mod_a, 1);
        // Scope Z: ast_a + mod_b (clone at AST only)
        insert_layer(&conn, "z", LayerKind::Ast, &ast_a, 1);
        insert_layer(&conn, "z", LayerKind::Module, &mod_b, 1);
        // Scope W: ast_b + mod_b (no shared layers)
        insert_layer(&conn, "w", LayerKind::Ast, &ast_b, 1);
        insert_layer(&conn, "w", LayerKind::Module, &mod_b, 1);

        let (hv_x, _) = build_combined_for_scope(&conn, "x").unwrap();
        let (hv_y, _) = build_combined_for_scope(&conn, "y").unwrap();
        let (hv_z, _) = build_combined_for_scope(&conn, "z").unwrap();
        let (hv_w, _) = build_combined_for_scope(&conn, "w").unwrap();

        let d_xy = popcount_distance(&hv_x, &hv_y);
        let d_xz = popcount_distance(&hv_x, &hv_z);
        let d_xw = popcount_distance(&hv_x, &hv_w);

        assert_eq!(d_xy, 0, "X and Y are identical at all layers");
        assert!(
            d_xz < d_xw,
            "X-Z (1 shared layer) should be closer than X-W (0 shared): \
             d(X,Z)={d_xz} vs d(X,W)={d_xw}",
        );
    }
}
