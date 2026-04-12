# Open Daemon Design: UDS Protocol + Extension Trait

**Date:** 2026-04-10
**Status:** Draft
**Depends on:** Extensible CLI (2026-04-08, complete)

## Problem

LLO's `leyline serve` is a dumb mount — arena + FUSE/NFS, no coordination socket. Mache needs a UDS socket for coordination (reparse after splice, arena flip detection, status queries). Currently only ley-line (private) has a daemon with a socket, but 80% of the daemon logic uses no private deps.

## Design Principle

LLO owns the daemon and the protocol. LL plugs in private ops. Mache talks to the socket — doesn't know or care what's behind it.

## Architecture

### The Daemon

A new `Daemon` subcommand in `leyline-cli-lib::Commands` that:

1. Creates/opens arena + controller
2. Mounts via NFS or FUSE (same as `serve`)
3. Starts a UDS socket at `<arena>.sock`
4. Symlinks to `~/.mache/default.sock` for auto-discovery
5. Handles line-delimited JSON ops
6. Runs event router for pub/sub notifications
7. Waits for shutdown (Ctrl+C / timeout / heartbeat)

### Base Ops (LLO — no private deps)

```
{"op": "status"}              → generation, arena path, arena size
{"op": "flush"}               → force generation bump
{"op": "load", "db": "<b64>"} → write .db into arena, bump gen
{"op": "query", "sql": "..."}  → run SQL against active buffer
{"op": "reparse", "path": "..."} → re-parse source file, update .db, bump gen
{"op": "subscribe"}           → start receiving push events
{"op": "unsubscribe"}         → stop receiving push events
```

### Extension Trait

```rust
/// Extension point for ley-line (private) to register additional ops.
pub trait DaemonExt: Send + Sync {
    /// Handle an op the base daemon doesn't recognize.
    /// Return Some(json_string) if handled, None to fall through to "unknown op".
    fn handle_op(&self, op: &str, req: &serde_json::Value) -> Option<String>;

    /// Async variant for ops that need .await (e.g., LSP tool invocation).
    fn handle_op_async(
        &self,
        op: &str,
        req: &serde_json::Value,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>>> {
        let _ = (op, req);
        None
    }
}
```

LL implements this to add sheaf_*, semantic_search, embed_*, tool ops.

### Dispatch Flow

```
Client sends: {"op": "sheaf_invalidate", ...}
                    │
                    ▼
            Base handler checks known ops
                    │
                 not found
                    │
                    ▼
            ext.handle_op("sheaf_invalidate", req)
                    │
                 Some(resp) → return to client
                 None       → {"error": "unknown op: sheaf_invalidate"}
```

### Event Router

Pub/sub for push notifications. Base events:

- `db_updated` — arena generation bumped (after load/reparse)
- `shutdown` — daemon is shutting down

LL adds: `sheaf_defect_changed`, `embed_complete`, etc.

### File Layout

```
rs/ll-open/cli-lib/src/
  cmd_daemon.rs     # Daemon entry point, arena setup, shutdown
  daemon/
    socket.rs       # UDS listener, line-delimited JSON dispatch
    ops.rs          # Base op handlers (status, flush, load, query, reparse)
    events.rs       # Event router (pub/sub)
    ext.rs          # DaemonExt trait definition
```

### CLI Args

```
leyline daemon \
  --arena ./project.arena \
  --arena-size-mib 64 \
  --control ./project.ctrl \
  --mount /tmp/mnt \
  --backend nfs \
  --nfs-port 0 \
  --language go \
  --timeout 1h
```

Same args as `serve` plus the UDS socket is always started. `serve` could become an alias for `daemon` or stay as the "no socket" variant.

### What Moves from ley-line

| From ley-line | To LLO | Notes |
|---|---|---|
| `spawn_uds_listener()` | `daemon/socket.rs` | Remove sheaf/embed params |
| `handle_uds_op()` base cases | `daemon/ops.rs` | status, flush, load, query |
| `event_router.rs` | `daemon/events.rs` | Generic, no private deps |
| `load_into_arena()` | Already in `cmd_load.rs` | Reuse |
| `query_arena()` | Already in `cmd_inspect.rs` | Reuse |

### What Stays in ley-line

| Component | Why |
|---|---|
| `sheaf_ops.rs` | Depends on leyline-sheaf |
| `net_control.rs` | TCP control channel for remote conductor |
| Heartbeat tracker | Remote conductor feature |
| Embed state / loop | Depends on leyline-embed |
| Inference engine | Depends on leyline-infer |
| Network receiver | Depends on leyline-net |

### Three Tiers (Updated)

| Tier | Binary | Socket | Ops |
|---|---|---|---|
| mache only | — | — | mache's own CGO path |
| mache + llo | `leyline` (open) | `default.sock` | status, flush, load, query, reparse |
| mache + ll | `leyline` (extended) | `default.sock` | All above + sheaf_*, semantic_search, embed_*, tool |

### Mache Integration

Mache already connects to `~/.mache/default.sock`. No mache protocol changes needed for base ops. Mache just starts using `reparse` and `subscribe` ops.

## What This Does NOT Cover

- LL-side cutover (LL implements DaemonExt, removes duplicated daemon code)
- Mache-side bundling (mache-33dc5f)
- ASTWalker parity (mache-36d961)
- Incremental reparse (only full file reparse for now)
