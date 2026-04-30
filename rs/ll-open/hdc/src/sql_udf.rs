//! Custom SQLite UDFs for HDC: popcount-distance and BUNDLE aggregate.
//!
//! Two functions register on a `rusqlite::Connection`:
//!
//! 1. `popcount_xor(a BLOB, b BLOB) -> INTEGER` — Hamming distance over
//!    the XOR of two equal-length BLOBs. Used by every radius/density
//!    query so the heavy bit-counting runs at C speed inside SQLite,
//!    not in Rust per-row.
//!
//! 2. `BUNDLE(hv BLOB) -> BLOB` aggregate — XOR-bundles a column of
//!    hypervectors at SQL time. Lets the engine roll up a layer
//!    (function HVs → file HV → directory HV → repo HV) without
//!    pulling rows into the host process. Per the user's
//!    "table-as-LEGO" insight (premise fact-check in epic
//!    ley-line-open-96b1a9). XOR-bundling is associative, commutative,
//!    self-inverse — the order rows arrive doesn't matter.
//!
//! Note on "bundle" semantics: math-friend review said majority-rule
//! bundling saturates around N≈250 at D=8192. **XOR-bundling** is
//! capacity-unbounded but represents *symmetric difference* rather
//! than *set membership*. Use it for hierarchical content addressing
//! (where you want "did anything change?" semantics, not "what's the
//! shared content?"). For a true majority-rule aggregate that
//! preserves set-bundle semantics within capacity, see `BUNDLE_MAJORITY`
//! below.

use rusqlite::{
    functions::{Aggregate, FunctionFlags},
    types::Value,
    Connection, Error, Result,
};

use crate::D_BYTES;

/// Register `popcount_xor`, `BUNDLE`, and `BUNDLE_MAJORITY` on a
/// connection. Idempotent in spirit: re-registering replaces the
/// previous registration. Call once per Connection (typically at
/// daemon startup or test setup).
pub fn register_hdc_udfs(conn: &Connection) -> Result<()> {
    // popcount_xor(a, b) — scalar function returning Hamming distance.
    conn.create_scalar_function(
        "popcount_xor",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
        |ctx| {
            let a = ctx.get::<Vec<u8>>(0)?;
            let b = ctx.get::<Vec<u8>>(1)?;
            if a.len() != b.len() {
                return Err(Error::UserFunctionError(
                    format!(
                        "popcount_xor: length mismatch ({} vs {})",
                        a.len(),
                        b.len(),
                    )
                    .into(),
                ));
            }
            let mut acc: u32 = 0;
            // Process 8 bytes at a time as u64 popcount when possible.
            let chunks_a = a.chunks_exact(8);
            let chunks_b = b.chunks_exact(8);
            let rem_a = chunks_a.remainder();
            let rem_b = chunks_b.remainder();
            for (ca, cb) in chunks_a.zip(chunks_b) {
                let xa = u64::from_le_bytes(ca.try_into().unwrap());
                let xb = u64::from_le_bytes(cb.try_into().unwrap());
                acc += (xa ^ xb).count_ones();
            }
            for (&ba, &bb) in rem_a.iter().zip(rem_b) {
                acc += (ba ^ bb).count_ones();
            }
            Ok(acc as i64)
        },
    )?;

    // BUNDLE(hv) — XOR-aggregate. SELECT BUNDLE(hv) FROM _hdc WHERE ...
    // Returns the XOR of every row; identical-content rows cancel out
    // (self-inverse), distinct content accumulates.
    conn.create_aggregate_function(
        "BUNDLE",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
        BundleXorAgg::new(),
    )?;

    // BUNDLE_MAJORITY(hv) — majority-rule aggregate. Bit i is 1 iff
    // more than half the input rows have bit i set. Within HDC capacity
    // (~250 items at D=8192), this preserves "set-membership" semantics:
    // BUNDLE_MAJORITY(items) ⊕ x ≈ items_without_x_in_them. Above
    // capacity, output saturates near random.
    conn.create_aggregate_function(
        "BUNDLE_MAJORITY",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
        BundleMajorityAgg::new(),
    )?;

    Ok(())
}

/// XOR-bundle aggregate state.
struct BundleXorAgg;

impl BundleXorAgg {
    fn new() -> Self {
        BundleXorAgg
    }
}

impl Aggregate<Vec<u8>, Value> for BundleXorAgg {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> Result<Vec<u8>> {
        // Start with the canonical zero hypervector. Every XOR step
        // either flips or preserves bits.
        Ok(vec![0u8; D_BYTES])
    }

    fn step(&self, ctx: &mut rusqlite::functions::Context<'_>, acc: &mut Vec<u8>) -> Result<()> {
        let row: Vec<u8> = ctx.get(0)?;
        if row.len() != acc.len() {
            return Err(Error::UserFunctionError(
                format!(
                    "BUNDLE: row length {} != accumulator length {}",
                    row.len(),
                    acc.len(),
                )
                .into(),
            ));
        }
        for (a, b) in acc.iter_mut().zip(row.iter()) {
            *a ^= *b;
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Vec<u8>>,
    ) -> Result<Value> {
        Ok(match acc {
            Some(bytes) => Value::Blob(bytes),
            // Empty input set → return NULL, mirroring SUM() over zero rows.
            None => Value::Null,
        })
    }
}

/// Majority-rule bundle aggregate state. Counts ones per bit, then
/// thresholds at finalize.
struct BundleMajorityAgg;

impl BundleMajorityAgg {
    fn new() -> Self {
        BundleMajorityAgg
    }
}

/// Per-row state: parallel u32 counters per bit, plus the row count.
struct MajorityState {
    counts: Vec<u32>,
    n_rows: u32,
}

impl Aggregate<MajorityState, Value> for BundleMajorityAgg {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> Result<MajorityState> {
        Ok(MajorityState {
            counts: vec![0u32; D_BYTES * 8],
            n_rows: 0,
        })
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        state: &mut MajorityState,
    ) -> Result<()> {
        let row: Vec<u8> = ctx.get(0)?;
        if row.len() != D_BYTES {
            return Err(Error::UserFunctionError(
                format!(
                    "BUNDLE_MAJORITY: row length {} != D_BYTES {}",
                    row.len(),
                    D_BYTES,
                )
                .into(),
            ));
        }
        for (byte_idx, &b) in row.iter().enumerate() {
            for bit_idx in 0..8 {
                if (b >> bit_idx) & 1 == 1 {
                    state.counts[byte_idx * 8 + bit_idx] += 1;
                }
            }
        }
        state.n_rows += 1;
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        state: Option<MajorityState>,
    ) -> Result<Value> {
        match state {
            None => Ok(Value::Null),
            Some(s) if s.n_rows == 0 => Ok(Value::Null),
            Some(s) => {
                let half = s.n_rows / 2;
                let mut out = vec![0u8; D_BYTES];
                for (bit_idx, &cnt) in s.counts.iter().enumerate() {
                    // Strictly greater than half is "majority". Ties
                    // (cnt == half) on even row counts go to 0; this
                    // matches Plate's convention.
                    if cnt > half {
                        out[bit_idx / 8] |= 1 << (bit_idx % 8);
                    }
                }
                Ok(Value::Blob(out))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{expand_seed, popcount_distance, xor_into};
    use crate::Hypervector;

    fn fixture_conn() -> Connection {
        let conn = crate::test_util::conn_with_udfs();
        // Test schema: a tiny hv-blob table.
        conn.execute_batch("CREATE TABLE hvs (id INTEGER PRIMARY KEY, hv BLOB);")
            .unwrap();
        conn
    }

    fn insert_hv(conn: &Connection, id: i64, hv: &Hypervector) {
        conn.execute("INSERT INTO hvs(id, hv) VALUES (?1, ?2)", (id, hv.to_vec()))
            .unwrap();
    }

    /// Run `SELECT BUNDLE(hv) FROM hvs` and unwrap to `Vec<u8>`.
    /// Replaces the ~6 verbatim copies of this query in the BUNDLE
    /// test cases. Use [`select_bundle_or_null`] when the test
    /// expects the empty-set NULL.
    fn select_bundle(conn: &Connection) -> Vec<u8> {
        conn.query_row("SELECT BUNDLE(hv) FROM hvs", [], |r| r.get(0))
            .unwrap()
    }

    /// Like [`select_bundle`] but returns `Option<Vec<u8>>` so an
    /// empty-set NULL doesn't blow up the test.
    fn select_bundle_or_null(conn: &Connection) -> Option<Vec<u8>> {
        conn.query_row("SELECT BUNDLE(hv) FROM hvs", [], |r| r.get(0))
            .unwrap()
    }

    /// `SELECT BUNDLE_MAJORITY(hv) FROM hvs` — non-empty input.
    fn select_bundle_majority(conn: &Connection) -> Vec<u8> {
        conn.query_row("SELECT BUNDLE_MAJORITY(hv) FROM hvs", [], |r| r.get(0))
            .unwrap()
    }

    /// `SELECT BUNDLE_MAJORITY(hv) FROM hvs` — may be empty (NULL).
    fn select_bundle_majority_or_null(conn: &Connection) -> Option<Vec<u8>> {
        conn.query_row("SELECT BUNDLE_MAJORITY(hv) FROM hvs", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn popcount_xor_zero_to_zero_is_zero() {
        let conn = fixture_conn();
        let zero = vec![0u8; D_BYTES];
        let d: i64 = conn
            .query_row("SELECT popcount_xor(?1, ?2)", (&zero, &zero), |r| r.get(0))
            .unwrap();
        assert_eq!(d, 0);
    }

    #[test]
    fn popcount_xor_matches_pure_rust_implementation() {
        // The UDF must produce byte-identical results to the
        // popcount_distance helper. If they diverge (e.g. UDF uses
        // big-endian u64 chunks), Hamming queries via SQL would
        // differ from queries via host code. Pin equality.
        let conn = fixture_conn();
        let a = expand_seed(0xDEAD_BEEF);
        let b = expand_seed(0xCAFE_BABE);
        let d_udf: i64 = conn
            .query_row(
                "SELECT popcount_xor(?1, ?2)",
                (a.to_vec(), b.to_vec()),
                |r| r.get(0),
            )
            .unwrap();
        let d_rust = popcount_distance(&a, &b) as i64;
        assert_eq!(d_udf, d_rust);
    }

    #[test]
    fn popcount_xor_length_mismatch_errors() {
        let conn = fixture_conn();
        let short = vec![0u8; 16];
        let long = vec![0u8; 32];
        let result: Result<i64> = conn.query_row(
            "SELECT popcount_xor(?1, ?2)",
            (&short, &long),
            |r| r.get(0),
        );
        assert!(result.is_err(), "length mismatch must error, not silently truncate");
    }

    #[test]
    fn bundle_xor_empty_set_is_null() {
        // Aggregate over zero rows returns NULL, matching SUM() and
        // friends. Callers can COALESCE if they want zero-vector
        // semantics for empty sets.
        let conn = fixture_conn();
        assert_eq!(select_bundle_or_null(&conn), None);
    }

    #[test]
    fn bundle_xor_single_row_returns_that_row() {
        let conn = fixture_conn();
        let hv = expand_seed(0x42);
        insert_hv(&conn, 1, &hv);
        assert_eq!(select_bundle(&conn), hv.to_vec());
    }

    #[test]
    fn bundle_xor_self_inverse_pair_cancels_to_zero() {
        // The load-bearing property: A XOR A = 0. Two identical rows
        // bundled together produce ZERO_HV. Pin so a refactor that
        // accidentally switched to OR or AND semantics is caught
        // immediately.
        let conn = fixture_conn();
        let hv = expand_seed(0xAAAA);
        insert_hv(&conn, 1, &hv);
        insert_hv(&conn, 2, &hv);
        assert_eq!(select_bundle(&conn), vec![0u8; D_BYTES]);
    }

    #[test]
    fn bundle_xor_three_rows_xors_all() {
        let conn = fixture_conn();
        let a = expand_seed(1);
        let b = expand_seed(2);
        let c = expand_seed(3);
        insert_hv(&conn, 1, &a);
        insert_hv(&conn, 2, &b);
        insert_hv(&conn, 3, &c);
        // Build expected: a XOR b XOR c.
        let mut expected = a;
        xor_into(&mut expected, &b);
        xor_into(&mut expected, &c);
        assert_eq!(select_bundle(&conn), expected.to_vec());
    }

    #[test]
    fn bundle_xor_order_independent() {
        // BUNDLE is commutative and associative — re-ordering rows
        // must not change the result.
        let conn1 = fixture_conn();
        let conn2 = fixture_conn();
        let hvs: Vec<_> = (0..10).map(expand_seed).collect();
        for (i, hv) in hvs.iter().enumerate() {
            insert_hv(&conn1, i as i64, hv);
        }
        for (i, hv) in hvs.iter().rev().enumerate() {
            insert_hv(&conn2, i as i64, hv);
        }
        let r1: Vec<u8> = conn1
            .query_row("SELECT BUNDLE(hv) FROM hvs ORDER BY id", [], |r| r.get(0))
            .unwrap();
        let r2: Vec<u8> = conn2
            .query_row("SELECT BUNDLE(hv) FROM hvs ORDER BY id", [], |r| r.get(0))
            .unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn bundle_majority_empty_set_is_null() {
        let conn = fixture_conn();
        assert_eq!(select_bundle_majority_or_null(&conn), None);
    }

    #[test]
    fn bundle_majority_single_row_returns_that_row() {
        let conn = fixture_conn();
        let hv = expand_seed(0xBEEF);
        insert_hv(&conn, 1, &hv);
        // 1 row, half = 0, so any cnt > 0 wins — i.e., bit set in input
        // is set in output. Result equals the input.
        assert_eq!(select_bundle_majority(&conn), hv.to_vec());
    }

    #[test]
    fn bundle_majority_three_identical_rows_returns_that_row() {
        // Three identical rows: every bit's count is either 0 or 3.
        // Half = 1, so cnt > 1 ⇒ output=1 iff input bit was set.
        let conn = fixture_conn();
        let hv = expand_seed(42);
        for i in 1..=3 {
            insert_hv(&conn, i, &hv);
        }
        assert_eq!(select_bundle_majority(&conn), hv.to_vec());
    }

    #[test]
    fn bundle_majority_recovers_repeated_member_within_capacity() {
        // The Plate "set-membership" property: BUNDLE_MAJORITY of N
        // distinct members M_i, where one (M_target) appears K times
        // and others appear once, recovers M_target's bits at high
        // signal. With N=10 and target appearing 7 times, the bundle
        // should be much closer to M_target than to a random vector.
        let conn = fixture_conn();
        let target = expand_seed(0xC0DE_C0DE);
        let other_hvs: Vec<_> = (1..10).map(expand_seed).collect();

        // Insert: 7 copies of target + 3 distinct others.
        let mut id = 0;
        for _ in 0..7 {
            id += 1;
            insert_hv(&conn, id, &target);
        }
        for hv in other_hvs.iter().take(3) {
            id += 1;
            insert_hv(&conn, id, hv);
        }

        let bundle_arr: Hypervector = select_bundle_majority(&conn).try_into().unwrap();

        // Target appears 7/10 = 70% — bundle should be very close to target.
        let d_target = popcount_distance(&bundle_arr, &target);
        // A distractor that appears only once should be far.
        let distractor = other_hvs[0];
        let d_distractor = popcount_distance(&bundle_arr, &distractor);

        assert!(
            d_target < d_distractor,
            "majority bundle should favor repeated member: \
             d(target)={d_target} vs d(distractor)={d_distractor}",
        );
    }

    #[test]
    fn bundle_majority_handles_ties_by_zero() {
        // Even row count with 50/50 split: ties go to 0. Pin so a
        // refactor that switched to "round up" or "use random" is
        // caught.
        let conn = fixture_conn();
        let mut a = vec![0u8; D_BYTES];
        let mut b = vec![0u8; D_BYTES];
        // a has bit 0 set; b doesn't.
        a[0] = 0b0000_0001;
        // b has bit 1 set; a doesn't.
        b[0] = 0b0000_0010;
        let a_arr: Hypervector = a.try_into().unwrap();
        let b_arr: Hypervector = b.try_into().unwrap();
        insert_hv(&conn, 1, &a_arr);
        insert_hv(&conn, 2, &b_arr);
        // 2 rows, half = 1, threshold cnt > 1. Bit 0: cnt=1 (only a),
        // not > 1, output 0. Bit 1: cnt=1 (only b), not > 1, output 0.
        assert_eq!(select_bundle_majority(&conn)[0], 0, "ties must resolve to 0");
    }

    #[test]
    fn bundle_xor_used_with_filter() {
        // Real-world usage: BUNDLE filtered by a layer or scope prefix.
        // SELECT BUNDLE(hv) FROM _hdc WHERE layer_kind='ast' AND
        // scope_id LIKE 'src/lib.rs/%' produces the file-level XOR
        // hypervector from all functions in that file. Pin the
        // filter+aggregate composition.
        let conn = fixture_conn();
        // Simulate: 3 rows for "src/a.rs", 2 for "src/b.rs".
        conn.execute_batch(
            "ALTER TABLE hvs ADD COLUMN scope TEXT;",
        )
        .unwrap();
        let hvs_a = [
            expand_seed(11),
            expand_seed(12),
            expand_seed(13),
        ];
        let hvs_b = [expand_seed(21), expand_seed(22)];
        for (i, hv) in hvs_a.iter().enumerate() {
            conn.execute(
                "INSERT INTO hvs(id, hv, scope) VALUES (?1, ?2, ?3)",
                (i as i64, hv.to_vec(), "src/a.rs"),
            )
            .unwrap();
        }
        for (i, hv) in hvs_b.iter().enumerate() {
            conn.execute(
                "INSERT INTO hvs(id, hv, scope) VALUES (?1, ?2, ?3)",
                (10 + i as i64, hv.to_vec(), "src/b.rs"),
            )
            .unwrap();
        }
        let bundle_a: Vec<u8> = conn
            .query_row(
                "SELECT BUNDLE(hv) FROM hvs WHERE scope = 'src/a.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Reconstruct expected: hvs_a[0] XOR hvs_a[1] XOR hvs_a[2]
        let mut expected = hvs_a[0];
        xor_into(&mut expected, &hvs_a[1]);
        xor_into(&mut expected, &hvs_a[2]);
        assert_eq!(bundle_a, expected.to_vec());
    }
}
