@0xa1b2c3d4e5f60001;

# Go codegen annotations (inert for capnpc-rust; consumed by capnpc-go).
using Go = import "/go.capnp";
$Go.package("daemon");
$Go.import("github.com/agentic-research/ley-line-open/clients/go/leyline-schema/daemon");

# Daemon protocol schema — the contract between ley-line daemon (Rust)
# and mache (Go) over the UDS control socket.
#
# This is the single source of truth. Both sides generate types from
# this file. Schema drift is a build error, not a runtime bug.

# ── Data types ────────────────────────────────────────────────────

struct Node {
  id       @0 :Text;
  parentId @1 :Text;
  name     @2 :Text;
  kind     @3 :Int32;   # 0=file, 1=dir
  size     @4 :Int64;
  record   @5 :Text;    # JSON-encoded content or metadata
}

struct Ref {
  nodeId   @0 :Text;
  sourceId @1 :Text;
}

struct ParseStats {
  parsed       @0 :UInt64;
  unchanged    @1 :UInt64;
  deleted      @2 :UInt64;
  errors       @3 :UInt64;
  changedFiles @4 :List(Text);
}

struct EnrichmentStats {
  passName       @0 :Text;
  filesProcessed @1 :UInt64;
  itemsAdded     @2 :UInt64;
  durationMs     @3 :UInt64;
}

struct Event {
  seq    @0 :UInt64;
  topic  @1 :Text;
  source @2 :Text;
  data   @3 :Text;   # JSON-encoded payload (flexible per topic)
}

struct QueryRow {
  values @0 :List(Text);
}

# ── Request / Response pairs ──────────────────────────────────────
#
# The UDS protocol is request-response (one request, one response per
# line). Each request has an "op" field. These structs define the
# typed shape for each op.

struct StatusResponse {
  # JSON wire uses snake_case (`arena_path`, `current_root`, etc.) —
  # consumer typed structs (Rust serde, hand-written Go) carry rename
  # tags. The capnp schema preserves camelCase per convention.
  #
  # `generation` was the pre-T2.4 sequence counter; current_root supersedes
  # it. The handler stopped emitting `generation` at the T2.4 cutover but
  # the field stays in the schema (per ADR-0014 §2 "removing fields is
  # never"). Decoders that see the field absent get the UInt64 default 0;
  # decoders that already used `generation` see 0 forever rather than a
  # stale counter.
  ok                @0 :Bool;
  generation        @1 :UInt64;
  arenaPath         @2 :Text;
  arenaSize         @3 :UInt64;
  phase             @4 :Text;
  currentRoot       @5 :Text;
  enrichment        @6 :Text;
  # JSON-encoded `{name → {last_run_at_ms?, basis?, error?}}`. Schema
  # leaves it as opaque Text for now; a typed EnrichmentMap follow-up
  # is tracked under a future bead. Consumers parse the inner JSON
  # by hand today.
  headSha           @7 :Text;
  lastReparseAtMs   @8 :Int64;
  # On the Cap'n Proto side this field is always present (capnp ints
  # can't be absent and default to 0). On the JSON wire we emit the
  # field only when populated — `last_reparse_at_ms` is omitted before
  # the first reparse, so clients distinguish "not yet" from epoch=0
  # by key presence. Cap'n Proto consumers should treat 0 as "not yet"
  # until a typed Optional follow-up lands.
  error             @9 :Text;
}

struct ReparseRequest {
  source @0 :Text;
  lang   @1 :Text;
}

struct ReparseResponse {
  # Wire emits a flat shape; `stats` was the original nested form (kept
  # per ADR-0014's "never remove" rule but no longer populated).
  ok           @0 :Bool;
  generation   @1 :UInt64;
  stats        @2 :ParseStats;
  currentRoot  @3 :Text;
  parsed       @4 :UInt64;
  unchanged    @5 :UInt64;
  deleted      @6 :UInt64;
  errors       @7 :UInt64;
  changedFiles @8 :List(Text);
}

struct SnapshotResponse {
  ok          @0 :Bool;
  generation  @1 :UInt64;
  currentRoot @2 :Text;
}

struct FlushRequest {}

struct FlushResponse {
  ok          @0 :Bool;
  currentRoot @1 :Text;
}

struct EnrichRequest {
  pass  @0 :Text;
  files @1 :List(Text);
}

struct EnrichResponse {
  ok          @0 :Bool;
  generation  @1 :UInt64;
  passes      @2 :List(EnrichmentStats);
  currentRoot @3 :Text;
}

struct LoadRequest {
  db @0 :Data;   # raw .db bytes (not base64 — capnp handles binary)
}

struct LoadResponse {
  ok          @0 :Bool;
  generation  @1 :UInt64;
  currentRoot @2 :Text;
}

struct QueryRequest {
  sql @0 :Text;
}

struct QueryResponse {
  ok      @0 :Bool;
  columns @1 :List(Text);
  rows    @2 :List(QueryRow);
}

struct ListChildrenRequest {
  id @0 :Text;
}

struct ListChildrenResponse {
  ok       @0 :Bool;
  children @1 :List(Node);
}

struct ReadContentRequest {
  id @0 :Text;
}

struct ReadContentResponse {
  ok      @0 :Bool;
  content @1 :Text;
  error   @2 :Text;
}

struct FindCallersRequest {
  token @0 :Text;
}

struct FindCallersResponse {
  ok      @0 :Bool;
  callers @1 :List(Ref);
}

struct FindDefsRequest {
  token @0 :Text;
}

struct FindDefsResponse {
  ok   @0 :Bool;
  defs @1 :List(Ref);
}

struct FindCalleesRequest {
  # The node whose forward references we want resolved to their definitions.
  # Mirrors FindCallersRequest's input slot but takes a node_id instead of a
  # token: `find_callers(token)` = "who refers to this?", `find_callees(id)`
  # = "what does this node refer to?"
  id @0 :Text;
}

struct FindCalleesResponse {
  ok      @0 :Bool;
  callees @1 :List(Ref);
}

struct TokenMapEntry {
  # One token → many node_ids. Used by both refs map and defs map bulk
  # responses. source_id is intentionally omitted — bulk consumers want
  # graph topology; per-token find_callers/find_defs still expose it.
  token   @0 :Text;
  nodeIds @1 :List(Text);
}

struct GetRefsMapRequest {}

struct GetRefsMapResponse {
  ok      @0 :Bool;
  entries @1 :List(TokenMapEntry);
}

struct GetDefsMapRequest {}

struct GetDefsMapResponse {
  ok      @0 :Bool;
  entries @1 :List(TokenMapEntry);
}

struct SchemaTier {
  # One tier in LLO's layer-ownership topology (ll-core / ll-open / future
  # extension-defined names). See docs/TABLE_CONTRACT.md "Layer Ownership".
  name   @0 :Text;
  crates @1 :List(Text);
}

struct GetSchemaRequest {}

struct GetSchemaResponse {
  ok    @0 :Bool;
  tiers @1 :List(SchemaTier);
}

struct GetDbPathRequest {}

struct GetDbPathResponse {
  # Filesystem paths the daemon owns. Used by mache for optional capnp
  # readthrough fast-paths (serve_lsp, serve_find_smells). Strictly
  # opt-in optimization — falling back to UDS query ops is always safe.
  ok           @0 :Bool;
  dbPath       @1 :Text;
  ctrlPath     @2 :Text;
  bindingsPath @3 :Text;
  astPath      @4 :Text;
  sourcePath   @5 :Text;
  headPath     @6 :Text;
}

struct GetNodeRequest {
  id @0 :Text;
}

struct GetNodeResponse {
  ok    @0 :Bool;
  node  @1 :Node;
  error @2 :Text;
}

struct SubscribeRequest {
  topics   @0 :List(Text);
  identity @1 :Text;
  since    @2 :UInt64;
}

struct SubscribeResponse {
  ok           @0 :Bool;
  headSeq      @1 :UInt64;
  replayCount  @2 :UInt64;
  replayGap    @3 :Bool;
}

struct ErrorResponse {
  error @0 :Text;
}
