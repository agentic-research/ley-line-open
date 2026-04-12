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
