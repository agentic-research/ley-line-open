//! The daemon's live database as a mount backing (ADR-0040 D4, bead
//! `ley-line-open-f2ee9f`).
//!
//! Before this, `leyline daemon --mount` built a `HotSwapGraph` over a
//! private deserialised copy of the arena. A FUSE write edited that copy and
//! `fsync` published it from there; `live.db` never saw the edit, so the next
//! `op_reparse`/`op_snapshot` republished `live.db` and the mount's hot-swap
//! dropped the edit (bead `192018`). Reading through a private copy also cost
//! four image-sized copies per save (bead `af6c9d`).
//!
//! [`LiveDbSource`] gives the mount what the daemon's own socket readers
//! have: pooled read connections to `live.db` (zero copies, the WAL serves
//! the committed state) and THE daemon writer for writes, so there is one
//! writer per arena and a mount write is durable the moment it commits.
//! `publish` is the daemon's own `snapshot_to_arena`, which is how
//! out-of-process consumers (mache, FFI, warm start) see the change.

use std::sync::Arc;

use anyhow::Result;
use leyline_fs::graph::LiveSource;
use rusqlite::Connection;

use super::DaemonContext;

/// [`LiveSource`] over a [`DaemonContext`]'s `live_db`.
pub struct LiveDbSource {
    ctx: Arc<DaemonContext>,
}

impl LiveDbSource {
    pub fn new(ctx: Arc<DaemonContext>) -> Self {
        Self { ctx }
    }
}

impl LiveSource for LiveDbSource {
    fn reader(&self) -> Result<Box<dyn std::ops::Deref<Target = Connection> + '_>> {
        let conn = self
            .ctx
            .live_db
            .reader_pool
            .get()
            .map_err(|e| anyhow::anyhow!("reader pool checkout failed: {e}"))?;
        Ok(Box::new(conn))
    }

    fn writer(&self) -> Box<dyn std::ops::DerefMut<Target = Connection> + '_> {
        Box::new(self.ctx.live_db.writer.lock())
    }

    fn publish(&self) -> Result<()> {
        self.ctx
            .with_write(|conn| crate::cmd_daemon::snapshot_to_arena(conn, &self.ctx.ctrl_path))
    }
}
