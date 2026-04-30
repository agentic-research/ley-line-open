//! Load command — writes a .db file into a ley-line arena via the Controller.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::{ArenaHeader, Controller, create_arena, write_to_arena};

/// CLI entry point: read the .db from disk, then delegate to [`load_into_arena`].
pub fn cmd_load(db: &Path, control: &Path) -> Result<()> {
    let db_bytes = fs::read(db).with_context(|| format!("read {}", db.display()))?;
    load_into_arena(control, &db_bytes)?;
    eprintln!(
        "loaded {} bytes into arena via {}",
        db_bytes.len(),
        control.display()
    );
    Ok(())
}

/// Write `db_bytes` into the arena managed by the Controller at `control`.
///
/// This is the reusable core that other commands (serve, daemon) can call
/// without going through the CLI surface.
///
/// Steps:
/// 1. Open (or create) the Controller.
/// 2. Read the arena path and size from the control block.
/// 3. Open the arena file via mmap.
/// 4. Initialize the arena header if fresh (magic == 0).
/// 5. Write to the inactive buffer and flip via [`write_to_arena`].
/// 6. Bump the generation in the Controller.
pub fn load_into_arena(control: &Path, db_bytes: &[u8]) -> Result<()> {
    let mut ctrl = Controller::open_or_create(control).context("open controller")?;

    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    anyhow::ensure!(
        !arena_path.is_empty(),
        "controller has no arena path — call set_arena first"
    );
    anyhow::ensure!(arena_size > 0, "controller arena size is 0");

    let buf_capacity = ArenaHeader::buffer_size(arena_size) as usize;
    anyhow::ensure!(
        db_bytes.len() <= buf_capacity,
        "db ({} bytes) exceeds arena buffer capacity ({} bytes)",
        db_bytes.len(),
        buf_capacity,
    );

    let mut mmap = create_arena(Path::new(&arena_path), arena_size)
        .context("open arena file")?;

    write_to_arena(&mut mmap, db_bytes).context("write to arena")?;

    let new_gen = ctrl.generation() + 1;
    ctrl.set_arena(&arena_path, arena_size, new_gen)
        .context("bump generation")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_errors_when_arena_path_unset() {
        // Scale-pin the explicit ensure! in load_into_arena. A
        // controller without an arena set is a misconfigured state
        // (e.g. a control file created via Controller::open_or_create
        // but never set_arena'd). Pin: load surfaces the
        // "no arena path" error rather than panicking on empty
        // string mmap.
        let td = TempDir::new().unwrap();
        let ctrl_path = td.path().join("test.ctrl");
        // Create the controller without setting the arena.
        let _ctrl = Controller::open_or_create(&ctrl_path).unwrap();
        let err = load_into_arena(&ctrl_path, &[0u8; 16])
            .expect_err("must error on no-arena-path");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("arena path") || msg.contains("set_arena"),
            "error must mention arena path; got: {msg}",
        );
    }

    #[test]
    fn load_errors_when_db_exceeds_arena_capacity() {
        // The registry-scale failure mode. A 1.1 GB ingest db loaded
        // into a 256 MB arena would ensure!-fail here. Pin the error
        // message so a misconfigured daemon/serve invocation surfaces
        // an actionable diagnostic rather than mmap-corruption when a
        // refactor dropped the bounds check.
        let td = TempDir::new().unwrap();
        let arena_path = td.path().join("small.arena");
        let ctrl_path = td.path().join("test.ctrl");
        // Arena tiny — minimum-sized so buffer_capacity is small.
        let arena_size = 4 * 1024; // 4 KB total → ~2 KB per buffer
        let _mmap = leyline_core::create_arena(&arena_path, arena_size).unwrap();
        let mut ctrl = Controller::open_or_create(&ctrl_path).unwrap();
        ctrl.set_arena(&arena_path.to_string_lossy(), arena_size, 0).unwrap();
        // Attempt to load 16 KB into ~2 KB buffer capacity.
        let too_big = vec![0u8; 16 * 1024];
        let err = load_into_arena(&ctrl_path, &too_big)
            .expect_err("must error on db exceeds capacity");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exceeds arena buffer capacity"),
            "error must mention capacity; got: {msg}",
        );
    }
}
