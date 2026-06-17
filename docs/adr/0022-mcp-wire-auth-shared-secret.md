# ADR-0022 — MCP wire auth: shared-secret token (local mode); cloister-proxied for remote

**Status:** Accepted (2026-06-17)
**Bead:** `ley-line-open-b8395d` (decision); `ley-line-open-b885d1` (implementation)
**Related:** `lectio/crates/memory-daemon/src/auth.rs` (precedent pattern); cloister ADR-0010 (vault slice model), cloister ADR-0019 (sign-only helper protocol)

---

## Context

The LLO daemon exposes an MCP HTTP listener (`rs/ll-open/cli-lib/src/daemon/mcp.rs`) bound by default to `127.0.0.1:8384`. The route `POST /mcp` accepts JSON-RPC and dispatches to the `tool_registry`'s op handlers. **There is currently no authentication on the wire.** The threat surface mirrors what lectio's `memory-daemon/src/auth.rs` documents:

- Any local process running as the same user can hit the listener.
- Any web page can probe `127.0.0.1:8384` via DNS rebinding (a stale `A`-record for an attacker-controlled domain pointed at 127.0.0.1).

The current public-bind gate (`ley-line-open-b7dd03`, `--mcp-allow-public`) keeps the listener loopback-only by default but does nothing for the same-user threat. The `--mcp-allow-public` opt-in (used inside the OCI image to publish `0.0.0.0:8384` for `docker -p 127.0.0.1:host:8384` forwarding) makes the gap more acute when the daemon runs in a network-reachable container.

The substrate-direction conversation 2026-06-17 (see closed bead `ley-line-open-dffb77` strategic-clarification thread) confirmed:

- LLO's job is **substrate for mache** — parse, serve, retrieve. Same-machine consumer.
- Lectio owns the personal-state surface (claude transcripts, agent logs, identity).
- Cloister owns the remote/multi-tenant perimeter (workerd vault DO, OIDC via Cloudflare Access, slice grants).

Given that landscape, LLO's auth model should be **as small as possible while closing the local threat.** No OIDC, no mTLS, no JWT. A shared-secret token in a header is sufficient for the same-machine threat and is the same model lectio ships in production.

## Decision

### Mode A (default, this ADR): shared-secret token gate

Adopt lectio's `auth.rs` pattern, ported to LLO's axum stack:

1. **Token location.** `~/.local/share/leyline/daemon.token` — 32 random bytes hex-encoded. File mode `0600` (user-only read/write).
2. **Token lifecycle.** Auto-generated at daemon startup if the file doesn't exist; reused otherwise. Rotation is out of scope for v1 (delete the file + restart the daemon to rotate).
3. **Wire format.** Every request to `POST /mcp` MUST include `x-leyline-token: <hex>`. Missing or non-matching → HTTP `401` with `{"error": "unauthorized"}` JSON.
4. **Comparison.** Constant-time via `subtle::ConstantTimeEq`. Length mismatch also returns 401 (no length-leak via timing).
5. **Middleware placement.** Axum `middleware::from_fn_with_state` wrapping the `/mcp` route. The `GET /mcp` SSE path (currently stub `405`) inherits the gate when it lands.
6. **UDS dispatch is NOT gated.** The UDS socket at `~/.leyline/daemon.sock` is filesystem-perm-gated already (`0600` parent directory); a process that can `connect(2)` is a process that can read the token file.
7. **Local-only by construction.** Token gate is wired only when `--mcp-port` is set. Pure-UDS daemons skip token bootstrap.

### Mode B (future ADR): cloister-proxied for remote access

Out of scope for this ADR but documented as the substrate-direction:

- When LLO needs to be reachable cross-machine, cloister fronts it via a workerd worker that handles OIDC (Cloudflare Access).
- Cloister authenticates the caller, then dispatches to LLO via a privileged path (service binding inside workerd, or a pre-shared token).
- LLO never grows its own remote-auth code. The "cloister authenticates; LLO trusts" boundary is enforced at the cloister DO layer per cloister ADR-0013 (slice grants).

A future ADR-0023 will spec the cloister↔LLO trust handshake when that work picks up. Until then, LLO's only auth model is this ADR.

## Rejected alternatives

- **OAuth / OIDC at the LLO layer.** Overkill for a same-machine substrate. Brings in client-credential management, token-refresh flows, and IdP coupling that aren't load-bearing for the substrate-for-mache use case. Cloister handles this for the remote case.
- **mTLS.** Cert generation + rotation + trust-store management is too much surface for a local daemon. mache's MCP client doesn't currently support mTLS either.
- **No auth (status quo).** The DNS-rebinding + same-user attack surface is real. lectio confronts the same threat and ships a token gate; LLO should do the same.
- **HMAC-signed requests.** Stronger than a shared-secret bearer token but more complex to implement (canonicalization of request bytes, replay protection via nonces or timestamps). Not justified for the same-machine threat model.
- **Token in URL query string.** Tokens in URLs leak through proxy logs, browser history, and `Referer` headers. Header-only is the safer surface.

## Consequences

### Positive

- **DNS-rebinding + same-user surface closed.** A web page or local process without the token file can't reach `/mcp` even on `127.0.0.1`.
- **Zero new substrate complexity.** ~150 LOC: file load + middleware + 4-5 tests.
- **Substrate boundary clear.** LLO does the local thing; cloister will do the remote thing. No overlap.
- **Pattern shared across the ecosystem.** LLO + lectio share the same auth shape; agents/operators learn one model.

### Negative

- **Rotation requires daemon restart.** Acceptable for a substrate process restarted occasionally; not acceptable for a Tier-1 service. The substrate isn't Tier-1.
- **Token file is in `~/.local/share/leyline/`.** That directory might not exist on a fresh install; daemon creates it.
- **`--mcp-no-auth` escape hatch.** Required for the first-run + container scenarios where there's no token file pre-provisioned. Logged as a warning. Out-of-image / out-of-CI use-only.

### Out of scope (future)

- Token rotation API (currently: delete + restart).
- Per-tool ACL (currently: token holder has full access).
- SSE event-stream auth (currently the `GET /mcp` path is a 405 stub; will inherit the same gate when it lands).
- Cloister↔LLO remote handshake (ADR-0023, deferred).

## Implementation notes (for bead `ley-line-open-b885d1`)

- New module `rs/ll-open/cli-lib/src/daemon/auth.rs` — `Token`, `load_or_generate(path)`, `require_token` middleware.
- Add `subtle = "2"` to `cli-lib/Cargo.toml` (currently transitive only).
- `DaemonContext` gets a `mcp_token: Option<Arc<String>>` field — `None` when MCP HTTP isn't running or `--mcp-no-auth` is set.
- `daemon::mcp::spawn` wires `middleware::from_fn_with_state` ahead of the `/mcp` route when a token is present.
- Tests cover: correct token → 200; missing header → 401; wrong token → 401; length mismatch → 401; UDS path unaffected.

## References

- `lectio/crates/memory-daemon/src/auth.rs` — the precedent pattern this ADR adopts.
- `rs/ll-open/cli-lib/src/daemon/mcp.rs` — the listener being gated.
- `cmd_daemon.rs` — `--mcp-port`, `--mcp-bind`, `--mcp-allow-public` flags (the public-bind gate is the precedent for the new `--mcp-no-auth` flag's shape).
- Cloister ADR-0010, ADR-0013, ADR-0019 — the remote / multi-tenant auth model that LLO deliberately does NOT adopt.
