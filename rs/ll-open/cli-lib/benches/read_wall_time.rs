//! ADR-0026 Phase 2.0 — F2 read-side measurement infrastructure
//! (bead `ley-line-open-335d34`).
//!
//! ── Purpose ───────────────────────────────────────────────────────────
//! Phase 2 of the pointer-store migration flips read paths from the
//! row-projected `_ast` schema to the content-addressed pointer store one
//! op at a time. Each stage gates on F2 (§9.2.4): pointer-store p99 read
//! time must be ≤ 50% of row-projected p99. Without a documented
//! row-projected baseline, F2 is unmeasurable and the migration ships
//! blind.
//!
//! This bench captures that baseline. Phase 2.0 is measurement only —
//! no consumer migration, no schema flip. Phases 2.1+ read the JSON
//! artifact this bench produces (`docs/research/read_wall_time_baseline.json`)
//! to decide whether to flip a given op from shadow → primary.
//!
//! ── What it measures ──────────────────────────────────────────────────
//! Four query shapes, each mapped to an existing LLO daemon op that
//! backs the mache query surface:
//!
//! | Bench shape       | LLO SQL path (this bench)           | mache-side name  |
//! |-------------------|-------------------------------------|-------------------|
//! | `get_overview`    | list_children(id="") — roots scan   | get_overview     |
//! | `find_definition` | node_defs WHERE token = ?           | find_definition  |
//! | `find_callers`    | node_refs WHERE token = ?           | find_callers     |
//! | `search`          | _ast WHERE node_kind = ? scan        | search           |
//!
//! Each shape runs N=10 000 iterations against an on-disk WAL SQLite db
//! seeded from the same 5-file Go fixture as
//! `tests/pointer_store_dual_write_test.rs`. Same fixture as Phase 1 so
//! the F1 (round-trip integrity) test surface and the F2 (wall-time)
//! bench surface stay tied to the same schema shape.
//!
//! ── Why not criterion? ────────────────────────────────────────────────
//! The existing WAL benches (`wal_concurrent_readers`, `wal_snapshot_experiment`)
//! use `harness = false` with `std::time::Instant` measurements. The
//! bench's job here is capturing p50/p99 wall-time for a JSON artifact
//! that ships in-tree — no A/B statistical comparison against a rolling
//! baseline. `Instant::now()` + sort + percentile matches that need
//! exactly and skips the criterion dev-dep.
//!
//! Run with:
//!     cargo bench --bench read_wall_time -p leyline-cli-lib
//!
//! Also prints a JSON summary to stdout — capture with:
//!     cargo bench --bench read_wall_time -p leyline-cli-lib 2>/dev/null \
//!         | sed -n '/^{$/,/^}$/p'

use std::fs;
use std::time::{Duration, Instant};

use leyline_cli_lib::cmd_parse::parse_into_conn;
use rusqlite::Connection;
use tempfile::TempDir;

// ── Fixture ────────────────────────────────────────────────────────────

/// Scale factor for the Go fixture: how many copies of each base file
/// to emit, each in its own subpackage. The Phase 1 F1 test uses N=1
/// (5 files, ~200 rows) — enough to catch schema-shape bugs but too
/// small to produce measurable read wall-time (the 5-file version came
/// in at 1-5µs p99, i.e. at the microsecond floor).
///
/// The bench needs enough rows that a 2× change is legible above the
/// timer floor. N=20 gives ~100 Go files / ~4 000 `_ast` rows / ~200
/// `node_defs` rows — enough that the SQL query planner does real work
/// and the p99 tail exists.
///
/// The task spec says "scale up ONE step but keep it fast" — this
/// keeps the whole bench under a second on macOS arm64 reference
/// hardware while producing p50/p99 numbers above the timer floor.
const FIXTURE_SCALE: usize = 20;

/// Amplified Go fixture — writes `FIXTURE_SCALE` copies of the same
/// 5-file base shape used in `pointer_store_dual_write_test::create_go_fixture`,
/// one per subdirectory (`pkg0/`, `pkg1/`, …). The content shape stays
/// identical to Phase 1's F1 fixture so bench-vs-test drift is a
/// content-only difference (file count), not a semantic one.
///
/// Each copy is emitted under a distinct package name (`pkg{i}`) so
/// the parser sees fresh identifier tokens per copy — otherwise the
/// content-addressed pointer store would dedup them all into one blob
/// (the whole point of ADR-0026's F4 dedup claim) and the row count
/// would stop growing. The token names (`add`, `sub`, `Point`, …)
/// STAY the same across copies, though — that's what gives
/// `find_definition` / `find_callers` a bound token with meaningful
/// scan cost.
fn create_go_fixture() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    for i in 0..FIXTURE_SCALE {
        let pkg_dir = dir.path().join(format!("pkg{i}"));
        fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let pkg_name = format!("pkg{i}");
        // main.go — one call site per package.
        fs::write(
            pkg_dir.join("main.go"),
            format!(
                "package {pkg_name}\n\nimport \"fmt\"\n\nfunc Main() {{\n\tfmt.Println(add(1, 2))\n}}\n"
            ),
        )
        .expect("write main.go");
        // util.go — `add` and `sub` definitions land in every package,
        // so `find_definition("add")` returns FIXTURE_SCALE rows.
        fs::write(
            pkg_dir.join("util.go"),
            format!(
                "package {pkg_name}\n\nfunc add(a, b int) int {{\n\treturn a + b\n}}\n\nfunc sub(a, b int) int {{\n\treturn a - b\n}}\n"
            ),
        )
        .expect("write util.go");
        // types.go — struct declarations widen the AST-node count
        // per package.
        fs::write(
            pkg_dir.join("types.go"),
            format!(
                "package {pkg_name}\n\ntype Point struct {{\n\tX int\n\tY int\n}}\n\ntype Vec struct {{\n\tDX int\n\tDY int\n}}\n"
            ),
        )
        .expect("write types.go");
        fs::write(
            pkg_dir.join("iface.go"),
            format!("package {pkg_name}\n\ntype Adder interface {{\n\tAdd(a, b int) int\n}}\n"),
        )
        .expect("write iface.go");
        fs::write(
            pkg_dir.join("consts.go"),
            format!("package {pkg_name}\n\nconst Pi = 3\n\nvar Origin = Point{{X: 0, Y: 0}}\n"),
        )
        .expect("write consts.go");
    }
    dir
}

/// Seed a WAL SQLite db on disk with the 5-file Go fixture. WAL because
/// that's the production daemon shape (WAL 15b — `ley-line-open-f0239d`);
/// running the bench under DELETE journal would measure something
/// nobody in production sees.
fn seed_db(db_path: &std::path::Path) -> Connection {
    let conn = Connection::open(db_path).expect("open db");
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .expect("set WAL");
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "PRAGMA journal_mode = WAL did not stick (got '{mode}')"
    );
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();

    let src = create_go_fixture();
    let r = parse_into_conn(&conn, src.path(), Some("go"), None).expect("parse fixture into db");
    let expected = (FIXTURE_SCALE * 5) as u64;
    assert_eq!(
        r.parsed, expected,
        "amplified fixture must parse {expected} Go files ({FIXTURE_SCALE} copies × 5 base files)"
    );
    // Keep TempDir alive until parse completes.
    drop(src);
    conn
}

/// Pick a representative token that exists in BOTH `node_defs` and
/// `node_refs` — otherwise `find_callers` benches against a
/// zero-row result set and the numbers collapse to the timer floor
/// (which is real but uninformative for the F2 gate). Falls back to
/// the literal string `"add"` — a function name defined in every
/// fixture package and called from every `Main()` — if the join
/// returns nothing.
fn pick_token(conn: &Connection) -> String {
    conn.query_row(
        "SELECT d.token FROM node_defs d \
         JOIN node_refs r ON r.token = d.token \
         GROUP BY d.token \
         ORDER BY COUNT(*) DESC \
         LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "add".to_string())
}

// ── Percentile helpers ────────────────────────────────────────────────

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx]
}

// ── Per-shape measurement ─────────────────────────────────────────────

/// Result of one bench shape.
#[derive(Debug)]
struct ShapeResult {
    name: &'static str,
    iterations: usize,
    p50: Duration,
    p99: Duration,
}

impl ShapeResult {
    fn as_json_fragment(&self) -> String {
        format!(
            "    \"{name}\": {{ \"p50_us\": {p50}, \"p99_us\": {p99}, \"iterations\": {iters} }}",
            name = self.name,
            p50 = self.p50.as_micros(),
            p99 = self.p99.as_micros(),
            iters = self.iterations,
        )
    }
}

/// Run one measurement shape N times against `conn`, returning p50/p99.
///
/// The closure runs the raw SQL that the corresponding op runs. Prepared
/// statements live in a `prepare_cached` cache in the daemon; recreating
/// the statement inside the closure would over-charge the read path with
/// SQL-parse cost. Prepare once, execute N times — matches the daemon
/// shape where consecutive requests hit the cached statement.
fn measure_shape<F>(
    name: &'static str,
    conn: &Connection,
    iterations: usize,
    mut op: F,
) -> ShapeResult
where
    F: FnMut(&Connection),
{
    // Warmup: 100 iters so the SQLite page cache / prepared-statement
    // cache warm up before we start recording. Removes the first-hit
    // outlier that would otherwise dominate the p99 on a 10k-sample
    // run.
    for _ in 0..100 {
        op(conn);
    }

    let mut samples: Vec<Duration> = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t0 = Instant::now();
        op(conn);
        samples.push(t0.elapsed());
    }

    let p50 = percentile(&mut samples, 0.50);
    let p99 = percentile(&mut samples, 0.99);
    ShapeResult {
        name,
        iterations,
        p50,
        p99,
    }
}

// ── Shapes ─────────────────────────────────────────────────────────────

/// `get_overview` — the root children listing that mache uses for its
/// top-level overview surface. Same SQL as `op_list_children("")`:
/// `SELECT id, parent_id, name, kind, size FROM nodes WHERE parent_id = ?
///  ORDER BY name`.
fn bench_get_overview(conn: &Connection, iters: usize) -> ShapeResult {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent_id, name, kind, size \
             FROM nodes WHERE parent_id = ?1 ORDER BY name",
        )
        .unwrap();
    measure_shape("get_overview", conn, iters, |_| {
        let rows: Vec<(String, String, String, i32, i64)> = stmt
            .query_map([""], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Consume rows — this is the same row materialization
        // `op_list_children` performs before capnp-encode. Preventing
        // it from being optimized away.
        std::hint::black_box(rows);
    })
}

/// `find_definition` — the `node_defs`-by-token lookup that backs
/// `op_find_defs` (mache's `find_definition`). Prepared once with token
/// bound at execute time; same SQL as `query_token_refs(..., "node_defs")`.
fn bench_find_definition(conn: &Connection, iters: usize, token: &str) -> ShapeResult {
    let mut stmt = conn
        .prepare_cached("SELECT node_id, source_id FROM node_defs WHERE token = ?1")
        .unwrap();
    measure_shape("find_definition", conn, iters, |_| {
        let rows: Vec<(String, String)> = stmt
            .query_map([token], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        std::hint::black_box(rows);
    })
}

/// `find_callers` — the `node_refs`-by-token lookup that backs
/// `op_find_callers`. Same SQL shape as `find_definition` but against
/// the refs table.
fn bench_find_callers(conn: &Connection, iters: usize, token: &str) -> ShapeResult {
    let mut stmt = conn
        .prepare_cached("SELECT node_id, source_id FROM node_refs WHERE token = ?1")
        .unwrap();
    measure_shape("find_callers", conn, iters, |_| {
        let rows: Vec<(String, String)> = stmt
            .query_map([token], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        std::hint::black_box(rows);
    })
}

/// `search` — a whole-`_ast`-scan by `node_kind`. Stands in for mache's
/// unstructured search: the row-projected schema doesn't have a
/// text/vec index in this bench (the vec/text-search features are
/// off in the workspace build), so the analog for §9.2 measurement is
/// the row-projected scan across the AST table. Whatever the pointer
/// store's answer is for "search", it has to beat THIS wall-time by 2×.
fn bench_search(conn: &Connection, iters: usize) -> ShapeResult {
    let mut stmt = conn
        .prepare_cached("SELECT node_id, source_id, node_kind FROM _ast WHERE node_kind = ?1")
        .unwrap();
    measure_shape("search", conn, iters, |_| {
        let rows: Vec<(String, String, String)> = stmt
            .query_map(["function_declaration"], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        std::hint::black_box(rows);
    })
}

// ── JSON emitter ───────────────────────────────────────────────────────

fn hardware_string() -> String {
    // uname -srm — kernel + arch. Best-effort; the JSON schema
    // documents this is a coarse hardware string, not a full spec.
    std::process::Command::new("uname")
        .args(["-srm"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn today_iso() -> String {
    // Match the ADR's date-stamp format (YYYY-MM-DD). Fall back to
    // epoch-day if `date` isn't available; the JSON artifact's
    // primary key is captured_at so both are legible.
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn print_json(results: &[ShapeResult]) {
    println!("{{");
    println!("  \"captured_at\": \"{}\",", today_iso());
    println!("  \"hardware\": \"{}\",", hardware_string());
    // Read from the workspace Cargo.toml at compile time — same
    // technique cmd_daemon uses for the version wire.
    println!("  \"leyline_version\": \"{}\",", env!("CARGO_PKG_VERSION"));
    println!("  \"read_path\": \"row-projected (_ast)\",");
    println!("  \"measurements\": {{");
    let last_idx = results.len().saturating_sub(1);
    for (i, r) in results.iter().enumerate() {
        let comma = if i == last_idx { "" } else { "," };
        println!("{}{}", r.as_json_fragment(), comma);
    }
    println!("  }}");
    println!("}}");
}

// ── Driver ─────────────────────────────────────────────────────────────

fn main() {
    eprintln!("=== ADR-0026 Phase 2.0 — F2 read-side baseline ===");
    eprintln!("Fixture: 5-file Go fixture (from pointer_store_dual_write_test)");
    eprintln!("Read path: row-projected (_ast, node_defs, node_refs, nodes)");
    eprintln!("Iterations per shape: 10 000 (100 warmup + 10 000 measured)");
    eprintln!();

    let dir = TempDir::new().expect("create bench temp dir");
    let db_path = dir.path().join("read_wall_time.db");
    let conn = seed_db(&db_path);

    let token = pick_token(&conn);
    eprintln!("Bound token for find_definition / find_callers: {token:?}");
    eprintln!();

    // 10k iters — matches the F2 gate specification (§9.2.4 says
    // "Bench N=10k queries"). Warmup phase inside `measure_shape` is
    // additional.
    let iters = 10_000;

    let t_all = Instant::now();
    let r_overview = bench_get_overview(&conn, iters);
    let r_find_def = bench_find_definition(&conn, iters, &token);
    let r_find_callers = bench_find_callers(&conn, iters, &token);
    let r_search = bench_search(&conn, iters);
    let total_elapsed = t_all.elapsed();

    let results = [r_overview, r_find_def, r_find_callers, r_search];

    eprintln!("── Results (p50/p99 in µs) ──");
    for r in &results {
        eprintln!(
            "  {:<20} p50={:>7.0}µs  p99={:>7.0}µs  iters={}",
            r.name,
            r.p50.as_micros() as f64,
            r.p99.as_micros() as f64,
            r.iterations,
        );
    }
    eprintln!();
    eprintln!("Total bench wall-time: {:.2}s", total_elapsed.as_secs_f64());
    eprintln!();
    eprintln!("── JSON summary (copy into docs/research/read_wall_time_baseline.json) ──");

    print_json(&results);
}
