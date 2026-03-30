//! Integration test: compile the C smoke test and run it against a temp arena.
//!
//! Creates a real arena file + control file on disk, populates it with
//! a `nodes` table, then compiles `tests/smoke_ffi.c` against `libleyline_fs`
//! and invokes the resulting binary.

use anyhow::Result;
use leyline_core::{ArenaHeader, Controller};
use leyline_schema::create_schema;
use rusqlite::{Connection, DatabaseName};
use std::process::Command;
use tempfile::TempDir;

/// Build a minimal arena file: [Header][Buf0][Buf1] with a nodes-table DB
/// in the active buffer, plus a control file pointing at it.
fn create_test_arena(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let source = Connection::open_in_memory()?;
    create_schema(&source)?;
    source.execute_batch(
        "INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns', '', 'vulns', 1, 0, 1000, NULL);
        INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns/CVE-1', 'vulns', 'CVE-1', 0, 23, 2000, '{\"severity\":\"critical\"}');
        INSERT INTO nodes (id, parent_id, name, kind, size, mtime, record) VALUES ('vulns/CVE-2', 'vulns', 'CVE-2', 0, 10, 3000, '{\"severity\":\"high\"}');",
    )?;
    let serialized = source.serialize(DatabaseName::Main)?;
    let db_bytes = serialized.as_ref();

    let buf_size = db_bytes.len().max(4096);
    let arena_size = 4096 + buf_size * 2;
    let mut arena = vec![0u8; arena_size];

    // Header: active_buffer = 0
    let header = ArenaHeader {
        magic: ArenaHeader::MAGIC,
        version: ArenaHeader::VERSION,
        active_buffer: 0,
        padding: [0; 2],
        sequence: 1,
    };
    let header_bytes: &[u8] = bytemuck::bytes_of(&header);
    arena[..header_bytes.len()].copy_from_slice(header_bytes);

    // Write DB into buffer 0 (offset = 4096)
    arena[4096..4096 + db_bytes.len()].copy_from_slice(db_bytes);

    let arena_path = dir.join("test.arena");
    std::fs::write(&arena_path, &arena)?;

    // Create control file pointing at the arena
    let ctrl_path = dir.join("test.ctrl");
    let mut ctrl = Controller::open_or_create(&ctrl_path)?;
    ctrl.set_arena(arena_path.to_str().unwrap(), arena_size as u64, 1)?;

    Ok(ctrl_path)
}

#[test]
fn smoke_ffi_c_binary() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctrl_path = create_test_arena(tmp.path())?;

    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let c_src = crate_dir.join("tests/smoke_ffi.c");
    let include_dir = crate_dir.join("include");

    // Find the target directory (where libleyline_fs lives)
    let target_dir = crate_dir.join("../../target/debug");

    let out_binary = tmp.path().join("smoke_ffi");

    // Compile the C test — platform-specific linker flags
    let mut cc_args = vec![
        "-o".to_string(),
        out_binary.to_str().unwrap().to_string(),
        c_src.to_str().unwrap().to_string(),
        format!("-I{}", include_dir.display()),
        format!("-L{}", target_dir.display()),
        "-lleyline_fs".to_string(),
    ];
    if cfg!(target_os = "macos") {
        cc_args.extend([
            "-framework".to_string(),
            "Security".to_string(),
            "-framework".to_string(),
            "CoreFoundation".to_string(),
            "-lresolv".to_string(),
        ]);
    } else {
        // Linux: link pthread, dl, m (pulled in by rusqlite/fuser)
        cc_args.extend([
            "-lpthread".to_string(),
            "-ldl".to_string(),
            "-lm".to_string(),
        ]);
    }
    let compile = Command::new("cc").args(&cc_args).output()?;

    if !compile.status.success() {
        let stderr = String::from_utf8_lossy(&compile.stderr);
        panic!("C compilation failed:\n{}", stderr);
    }

    // Run the C test
    let run = Command::new(out_binary.to_str().unwrap())
        .arg(ctrl_path.to_str().unwrap())
        .env("DYLD_LIBRARY_PATH", target_dir.to_str().unwrap())
        .output()?;

    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);

    if !run.status.success() {
        panic!(
            "C smoke test failed (exit {}):\nstdout:\n{}\nstderr:\n{}",
            run.status, stdout, stderr
        );
    }

    println!("{}", stdout);
    assert!(
        stdout.contains("PASS"),
        "expected PASS in output:\n{}",
        stdout
    );

    Ok(())
}
