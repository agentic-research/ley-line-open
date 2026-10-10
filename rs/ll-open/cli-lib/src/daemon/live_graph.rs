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

#[cfg(test)]
mod tests {
    use super::*;

    /// `publish` is the daemon's snapshot: after it, the control block names
    /// the live database's image, so out-of-process readers see the write.
    #[tokio::test]
    async fn publish_advances_the_published_root_to_the_live_image() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::daemon::socket::tests::test_context(dir.path());
        ctx.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")?;
            Ok(())
        })
        .unwrap();
        let root = || {
            leyline_core::Controller::open_or_create(&ctx.ctrl_path)
                .unwrap()
                .current_root()
        };
        let before = root();
        let source = LiveDbSource::new(ctx.clone());
        source.publish().unwrap();
        let after = root();
        assert_ne!(after, before, "publish must advance current_root");
        assert_ne!(after, [0u8; 32]);

        // The source's reader sees the live db, and its writer is the daemon's.
        let n: i64 = source
            .reader()
            .unwrap()
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        source
            .writer()
            .execute("INSERT INTO t VALUES (2)", [])
            .unwrap();
        let n: i64 = ctx
            .with_read(|c| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 2);
    }
}
