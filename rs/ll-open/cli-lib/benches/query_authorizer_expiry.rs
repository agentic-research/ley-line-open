//! What the `query` op's authorizer costs the reader pool's other ops
//! (bead `ley-line-open-a9d50a`, N2).
//!
//! `sql_read_only::query` installs a SQLite authorizer on the checked-out
//! reader and removes it afterwards. `sqlite3_set_authorizer` expires every
//! prepared statement on the connection both times, so a statement another
//! op cached with `prepare_cached` is prepared again the next time it
//! steps. This bench measures that re-prepare against the cost of the ops
//! themselves.
//!
//! Arms, each on one reader-pool connection of a WAL live db holding a
//! 40-table schema (the order of the daemon's):
//!
//! - `cached`: a `prepare_cached` point lookup, alone. Its statement stays
//!   prepared.
//! - `cached_after_toggle`: the same lookup after installing and removing
//!   an authorizer, which is what a `query` op leaves behind.
//! - `query`: a `query` op (a point `SELECT`), alone.
//! - `catalog`: the two reads a `query` op makes before it installs the
//!   authorizer, alone.
//! - `interleaved`: a `query` op and then the lookup, per iteration.
//!
//! The re-prepare cost is `cached_after_toggle - cached`; `interleaved -
//! query - cached` is the same cost seen through the real op.
//!
//! Run: `cargo bench -p leyline-cli-lib --bench query_authorizer_expiry`.

use std::time::{Duration, Instant};

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::daemon::sql_read_only;
use rusqlite::Connection;
use rusqlite::hooks::{AuthContext, Authorization};

const ITERATIONS: u32 = 20_000;
const ROUNDS: u32 = 7;

fn main() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db = LiveDb::open_fresh_for_test(&dir.path().join("live.db"));
    {
        let writer = db.writer.lock();
        let mut ddl = String::from(
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 1000)
             INSERT INTO t SELECT x, 'row ' || x FROM n;",
        );
        for i in 0..39 {
            ddl.push_str(&format!(
                "CREATE TABLE extra_{i} (id INTEGER PRIMARY KEY, v TEXT);
                 CREATE INDEX extra_{i}_v ON extra_{i} (v);"
            ));
        }
        writer.execute_batch(&ddl).expect("schema");
    }
    let mut pooled = db.reader_pool.get().expect("reader");
    let conn: &mut Connection = &mut pooled;

    let cached = best(|| {
        for i in 0..ITERATIONS {
            lookup(conn, i);
        }
    });
    let after_toggle = best_with_setup(conn, |conn, i| {
        toggle(conn);
        let t = Instant::now();
        lookup(conn, i);
        t.elapsed()
    });
    let toggle_alone = best(|| {
        for _ in 0..ITERATIONS {
            toggle(conn);
        }
    });
    let query = best(|| {
        for i in 0..ITERATIONS {
            query_op(conn, i);
        }
    });
    let catalog = best(|| {
        for _ in 0..ITERATIONS {
            catalog_reads(conn);
        }
    });
    let interleaved = best(|| {
        for i in 0..ITERATIONS {
            query_op(conn, i);
            lookup(conn, i);
        }
    });

    let per = |d: Duration| d.as_secs_f64() * 1e9 / f64::from(ITERATIONS);
    println!("query_authorizer_expiry (ns per op, best of {ROUNDS} rounds of {ITERATIONS})");
    println!("  cached lookup                 {:>9.0}", per(cached));
    println!("  cached lookup after a toggle  {:>9.0}", per(after_toggle));
    println!("  authorizer install + remove   {:>9.0}", per(toggle_alone));
    println!("  query op                      {:>9.0}", per(query));
    println!("  its catalog reads             {:>9.0}", per(catalog));
    println!("  query op + cached lookup      {:>9.0}", per(interleaved));
    println!(
        "  re-prepare (after toggle - cached)            {:>9.0}",
        per(after_toggle) - per(cached)
    );
    println!(
        "  re-prepare (interleaved - query - cached)     {:>9.0}",
        per(interleaved) - per(query) - per(cached)
    );
    println!(
        "  re-prepare as a share of a query op           {:>8.1}%",
        100.0 * (per(after_toggle) - per(cached)) / per(query)
    );
}

/// Another op's point lookup through the statement cache.
fn lookup(conn: &Connection, i: u32) {
    let b: String = conn
        .prepare_cached("SELECT b FROM t WHERE a = ?1")
        .expect("prepare")
        .query_row([i64::from(i % 1000) + 1], |row| row.get(0))
        .expect("lookup");
    std::hint::black_box(b);
}

/// A `query` op's point `SELECT`.
fn query_op(conn: &mut Connection, i: u32) {
    let sql = format!("SELECT b FROM t WHERE a = {}", i % 1000 + 1);
    let n = sql_read_only::query(conn, &sql, |stmt| {
        let mut rows = stmt.query([])?;
        let mut n = 0;
        while rows.next()?.is_some() {
            n += 1;
        }
        Ok(n)
    })
    .expect("query");
    std::hint::black_box(n);
}

/// The two reads a `query` op makes before installing its authorizer (the
/// module allow-list's catalog, bead `ley-line-open-a9be1f`), as it makes
/// them.
fn catalog_reads(conn: &Connection) {
    for sql in [
        "SELECT name FROM pragma_module_list",
        "SELECT name, type = 'table' AND rootpage = 0 FROM main.sqlite_schema \
         WHERE type IN ('table', 'view')",
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let n = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .count();
        std::hint::black_box(n);
    }
}

/// Install and remove an authorizer, as a `query` op does around its
/// statement.
fn toggle(conn: &mut Connection) {
    conn.authorizer(Some(|_: AuthContext<'_>| Authorization::Allow))
        .expect("install");
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove");
}

fn best(mut run: impl FnMut()) -> Duration {
    run();
    (0..ROUNDS)
        .map(|_| {
            let t = Instant::now();
            run();
            t.elapsed()
        })
        .min()
        .expect("rounds")
}

/// The best round of `ITERATIONS` calls of `one`, which times its own
/// measured part.
fn best_with_setup(
    conn: &mut Connection,
    mut one: impl FnMut(&mut Connection, u32) -> Duration,
) -> Duration {
    (0..=ROUNDS)
        .map(|_| (0..ITERATIONS).map(|i| one(conn, i)).sum())
        .skip(1)
        .min()
        .expect("rounds")
}
