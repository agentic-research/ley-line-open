//! The bundled SQLite is at or past the RUSTSEC-2026-0459 fix (bead
//! `ley-line-open-6ef210`).
//!
//! SQLite before 3.54.0 can return uninitialized page-cache memory when a
//! database is opened with a `-wal` file of a different page size
//! (CVE-2026-119375). The workspace links one SQLite, built from the source
//! `libsqlite3-sys` bundles (rusqlite's `bundled` feature); rusqlite 0.41
//! bundles 3.54.x. A dependency change that brings back an older bundled
//! SQLite fails this test.

use rusqlite::Connection;

/// The first SQLite release with the CVE-2026-119375 fix.
const FLOOR: (u32, u32, u32) = (3, 54, 0);

fn parse(version: &str) -> (u32, u32, u32) {
    let mut parts = version.split('.').map(|p| {
        p.parse::<u32>()
            .unwrap_or_else(|_| panic!("SQLite version {version:?} is not dotted numbers"))
    });
    let mut next = || parts.next().unwrap_or(0);
    (next(), next(), next())
}

#[test]
fn bundled_sqlite_is_at_least_cve_fixed_version() {
    let conn = Connection::open_in_memory().unwrap();
    let reported: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .unwrap();
    assert!(
        parse(&reported) >= FLOOR,
        "SELECT sqlite_version() = {reported}; RUSTSEC-2026-0459 needs >= {}.{}.{}",
        FLOOR.0,
        FLOOR.1,
        FLOOR.2,
    );
    // The library the process linked agrees with what the SQL reports: one
    // SQLite, not a second copy behind another `libsqlite3-sys`.
    assert_eq!(rusqlite::version(), reported);
}

#[test]
fn version_parse_orders_releases() {
    assert_eq!(parse("3.54.1"), (3, 54, 1));
    assert_eq!(parse("3.54"), (3, 54, 0));
    assert!(parse("3.53.9") < FLOOR);
    assert!(parse("3.54.0") >= FLOOR);
    assert!(parse("3.100.0") > FLOOR);
}
