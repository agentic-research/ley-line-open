//! A run's workspace share served from a daemon's own context.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leyline_cli_lib::daemon::db_pool::LiveDb;
use leyline_cli_lib::daemon::{ContextSlot, DaemonContext, DaemonState, EventRouter, NoExt};
use leyline_cli_lib::source_write::Grant;
use leyline_runtime::backends::libkrun::share::{FsService, Reply, Request, WorkspaceShares};
use leyline_runtime::{ErrorCode, WorkspaceGrant, WorkspaceOperation};
use tempfile::TempDir;

use super::{DaemonShares, scope_for};
use crate::open_flags::{O_RDWR, O_TRUNC};
use crate::tests::{A, A_EDIT};
use crate::{Freshness, ROOT_INODE, Scope, Store};

const SETTLED: Duration = Duration::from_secs(9);

fn freshness() -> Freshness {
    Freshness {
        settled: SETTLED,
        changed: Duration::from_millis(300),
        window: Duration::from_secs(20),
    }
}

/// A daemon's context over a store parsed from a source tree holding
/// `a.go`, with that tree as its checkout and an arena to publish into.
/// Needs a Tokio runtime: the event router spawns its dispatch loop.
fn daemon(dir: &TempDir) -> Arc<DaemonContext> {
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.go"), A).unwrap();
    let ctrl_path = dir.path().join("d.ctrl");
    let arena = ctrl_path.with_extension("arena");
    leyline_core::create_arena(&arena, 2 * 1024 * 1024).unwrap();
    leyline_core::Controller::open_or_create(&ctrl_path)
        .unwrap()
        .set_arena(&arena.to_string_lossy(), 2 * 1024 * 1024)
        .unwrap();
    let live_db = LiveDb::open_fresh_for_test(&ctrl_path.with_extension("live.db"));
    live_db
        .with_write(|conn| {
            leyline_cli_lib::cmd_parse::parse_into_conn(conn, &src, None, None)?;
            leyline_cli_lib::checkout::ensure_tables(conn)
        })
        .unwrap();
    Arc::new(DaemonContext {
        ctrl_path,
        ext: Arc::new(NoExt),
        router: EventRouter::new(16),
        live_db,
        enrich_inflight: Default::default(),
        source_dir: Some(src),
        lang_filter: None,
        enrichment_passes: Vec::new(),
        state: Arc::new(parking_lot::RwLock::new(DaemonState::initializing())),
        sheaf: Arc::new(leyline_cli_lib::daemon::sheaf_ops::SheafState::new()),
    })
}

fn tree_root(ctx: &DaemonContext) -> String {
    let root = Store::with_read(ctx, |conn| {
        Ok(leyline_cli_lib::tree_root::DerivedFrom::read(conn)?
            .expect("a parsed store has a tree root")
            .tree_root)
    })
    .unwrap();
    format!("blake3-256:{root}")
}

fn grant(graph_root: String, operations: Vec<WorkspaceOperation>) -> WorkspaceGrant {
    WorkspaceGrant {
        name: "repo".into(),
        graph_root,
        expected_generation: 0,
        operations,
    }
}

fn published_root(ctrl: &Path) -> [u8; 32] {
    leyline_core::Controller::open_or_create(ctrl)
        .unwrap()
        .current_root()
}

fn lookup_a(share: &dyn FsService) -> u64 {
    match share.call(Request::Lookup {
        parent: ROOT_INODE,
        name: b"a.go".to_vec(),
    }) {
        Reply::Entry { entry } => entry.inode,
        other => panic!("expected a.go, got {other:?}"),
    }
}

fn nothing() -> Grant {
    Grant::Paths(BTreeSet::new())
}

#[test]
fn a_grants_operations_resolve_to_what_the_guest_sees_and_proposes() {
    use WorkspaceOperation::*;
    let scope = |see, propose| Scope { see, propose };
    for (operations, expected) in [
        (vec![Read], scope(Grant::Whole, nothing())),
        (vec![List], scope(Grant::Whole, nothing())),
        (vec![Query], scope(nothing(), nothing())),
        (vec![Mutate], scope(nothing(), Grant::Whole)),
        (vec![Commit], scope(nothing(), Grant::Whole)),
        (vec![Read, Mutate], scope(Grant::Whole, Grant::Whole)),
        (vec![List, Commit], scope(Grant::Whole, Grant::Whole)),
    ] {
        assert_eq!(scope_for(&operations), expected, "{operations:?}");
    }
}

#[tokio::test]
async fn a_share_opened_before_the_daemon_has_a_context_is_refused() {
    let shares = DaemonShares::new(ContextSlot::default(), freshness());
    let error = match shares.open("run-1", &grant(String::new(), vec![])) {
        Ok(_) => panic!("no daemon context yet"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::NotProvisioned);
    assert!(error.retryable);
}

#[tokio::test]
async fn a_share_whose_granted_root_the_store_no_longer_holds_is_refused() {
    let dir = TempDir::new().unwrap();
    let ctx = daemon(&dir);
    let slot = ContextSlot::default();
    slot.set(Arc::clone(&ctx)).ok().unwrap();
    let shares = DaemonShares::new(slot, freshness());
    let stale = format!("blake3-256:{}", "0".repeat(64));
    let error = match shares.open(
        "run-1",
        &grant(stale.clone(), vec![WorkspaceOperation::Read]),
    ) {
        Ok(_) => panic!("the store does not hold the granted root"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch);
    assert!(error.detail.contains(&stale), "{error:?}");
    assert!(error.detail.contains(&tree_root(&ctx)), "{error:?}");
}

/// The store records no commit generation (ADR-0041 S1 is not implemented),
/// so a grant that claims one is refused before the worker starts rather
/// than run against a generation nothing checked. The granted root is the
/// store's, so the generation alone refuses it.
#[tokio::test]
async fn a_share_whose_grant_claims_a_generation_the_store_cannot_verify_is_refused() {
    let dir = TempDir::new().unwrap();
    let ctx = daemon(&dir);
    let slot = ContextSlot::default();
    slot.set(Arc::clone(&ctx)).ok().unwrap();
    let shares = DaemonShares::new(slot, freshness());
    for generation in [1, 7, u64::MAX] {
        let claimed = WorkspaceGrant {
            expected_generation: generation,
            ..grant(tree_root(&ctx), vec![WorkspaceOperation::Read])
        };
        let error = match shares.open("run-1", &claimed) {
            Ok(_) => panic!("generation {generation} cannot be verified"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::IdentityPolicyMismatch);
        assert!(!error.retryable, "{error:?}");
        assert!(
            error
                .detail
                .contains(&format!("expectedGeneration {generation}")),
            "{error:?}"
        );
    }
    // Zero claims no generation: the granted root alone pins the run.
    assert!(
        shares
            .open(
                "run-1",
                &grant(tree_root(&ctx), vec![WorkspaceOperation::Read])
            )
            .is_ok()
    );
}

#[tokio::test]
async fn a_granted_share_serves_the_daemons_store_and_lands_as_its_proposals_do() {
    let dir = TempDir::new().unwrap();
    let ctx = daemon(&dir);
    let slot = ContextSlot::default();
    slot.set(Arc::clone(&ctx)).ok().unwrap();
    let shares = DaemonShares::new(slot, freshness());
    let share = shares
        .open(
            "run-1",
            &grant(
                tree_root(&ctx),
                vec![WorkspaceOperation::Read, WorkspaceOperation::Mutate],
            ),
        )
        .expect("the granted root is the store's");

    // The configured freshness reaches the guest.
    match share.call(Request::Lookup {
        parent: ROOT_INODE,
        name: b"a.go".to_vec(),
    }) {
        Reply::Entry { entry } => assert_eq!(entry.entry_ttl, SETTLED),
        other => panic!("expected a.go, got {other:?}"),
    }

    let before = published_root(&ctx.ctrl_path);
    let a = lookup_a(share.as_ref());
    let Reply::Opened { handle, .. } = share.call(Request::Open {
        inode: a,
        flags: O_RDWR | O_TRUNC,
    }) else {
        panic!("a write open");
    };
    share.call(Request::Write {
        handle,
        offset: 0,
        data: A_EDIT.as_bytes().to_vec(),
    });
    assert_eq!(share.call(Request::Flush { handle }), Reply::Done);

    // On the daemon's store, through its own reader pool...
    let stored = Store::with_read(ctx.as_ref(), |conn| {
        Ok(conn.query_row(
            "SELECT b.blob_bytes FROM _source s JOIN source_blobs b \
               ON b.blob_hash = s.content_hash WHERE s.id = 'a.go'",
            [],
            |r| r.get::<_, Vec<u8>>(0),
        )?)
    })
    .unwrap();
    assert_eq!(stored, A_EDIT.as_bytes());
    // ...in its checkout...
    assert_eq!(
        std::fs::read_to_string(ctx.source_dir.as_ref().unwrap().join("a.go")).unwrap(),
        A_EDIT
    );
    // ...and handed to the daemon's publisher, as a socket proposal is: the
    // flush asked for a publish and did not wait for one (bead
    // `ley-line-open-8c8ded`)...
    assert_eq!(published_root(&ctx.ctrl_path), before, "the flush did not publish");
    tokio::time::timeout(Duration::from_secs(5), ctx.live_db.publish_requested.notified())
        .await
        .expect("the landed proposal requested a publish");
    // ...and the publisher's next image carries it.
    leyline_cli_lib::cmd_daemon::publish_living_db(&ctx.live_db, &ctx.ctrl_path).unwrap();
    assert_ne!(published_root(&ctx.ctrl_path), before);
}

#[tokio::test]
async fn a_share_granted_only_reads_refuses_a_write_open() {
    let dir = TempDir::new().unwrap();
    let ctx = daemon(&dir);
    let slot = ContextSlot::default();
    slot.set(Arc::clone(&ctx)).ok().unwrap();
    let share = DaemonShares::new(slot, freshness())
        .open(
            "run-1",
            &grant(tree_root(&ctx), vec![WorkspaceOperation::Read]),
        )
        .unwrap();
    let a = lookup_a(share.as_ref());
    assert_eq!(
        share.call(Request::Open {
            inode: a,
            flags: O_RDWR
        }),
        Reply::Error { errno: 13 }
    );
}
