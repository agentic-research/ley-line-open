@0xa1b2c3d4e5f60001;

# Go codegen annotations (inert for capnpc-rust; consumed by capnpc-go).
using Go = import "/go.capnp";
$Go.package("daemon");
$Go.import("github.com/agentic-research/ley-line-open/clients/go/leyline-schema/daemon");

# Json codec annotations (consumed by capnp-json at runtime). The
# JSON wire uses snake_case; the schema preserves camelCase per
# capnp convention. $Json.name maps each camelCase field to its
# snake_case wire name. See `rs/ll-open/cli-lib/src/daemon/wire.rs`
# for the integration point and ADR-0014's interim-status note for
# why JSON-as-carrier is the current discipline.
using Json = import "/capnp/compat/json.capnp";

# Daemon protocol schema — the contract between ley-line daemon (Rust)
# and mache (Go) over the UDS control socket.
#
# This is the single source of truth. Both sides generate types from
# this file. Schema drift is a build error, not a runtime bug.

# ── Data types ────────────────────────────────────────────────────

struct Node {
  id       @0 :Text;
  parentId @1 :Text  $Json.name("parent_id");
  name     @2 :Text;
  kind     @3 :Int32;   # 0=file, 1=dir
  size     @4 :Int64;
  record   @5 :Text;    # JSON-encoded content or metadata
}

struct Ref {
  nodeId   @0 :Text  $Json.name("node_id");
  sourceId @1 :Text  $Json.name("source_id");
}

struct ParseStats {
  parsed       @0 :UInt64;
  unchanged    @1 :UInt64;
  deleted      @2 :UInt64;
  errors       @3 :UInt64;
  changedFiles @4 :List(Text)  $Json.name("changed_files");
}

struct EnrichmentStats {
  passName       @0 :Text  $Json.name("pass_name");
  filesProcessed @1 :UInt64  $Json.name("files_processed");
  itemsAdded     @2 :UInt64  $Json.name("items_added");
  durationMs     @3 :UInt64  $Json.name("duration_ms");
}

# Per-pass enrichment status — the *current* state of a registered
# enrichment pass (distinct from EnrichmentStats which is *per-run*
# stats). Populated from `DaemonState.enrichment` map values.
struct PassStatus {
  # Wall-clock millis when the pass last completed. capnp-json emits
  # Int64 fields as JSON strings; "0" means "not yet" (the pass has
  # never run on this daemon instance). Consumers ignore when "0".
  lastRunAtMs @0 :Int64  $Json.name("last_run_at_ms");
  # parse_version the pass last ran against (causal basis). Same "0
  # means not yet" convention.
  basis       @1 :UInt64;
  # Last error message, cleared on next successful run. Text field —
  # capnp-json omits when unset, so absence means "no error".
  error       @2 :Text;
}

# One entry in the per-pass enrichment status map. Used inside
# `StatusResponse.enrichmentTyped` to replace the legacy `enrichment
# @6 :Text` JSON-encoded-string field. Typed end-to-end; no double
# parse on consumers.
struct EnrichmentEntry {
  name   @0 :Text;
  status @1 :PassStatus;
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
  # the camelCase capnp field names map via `$Json.name(...)` annotations
  # consumed by capnp-json's codec at runtime.
  #
  # `generation` was the pre-T2.4 sequence counter; `current_root`
  # supersedes it. Post-b0ea2e (capnp-json wire codec): the field is
  # emitted on every status response as the UInt64 default `"0"` —
  # capnp-json emits all primitive fields including defaults, and
  # ADR-0014 §2 forbids removing the ordinal. Handlers never set it;
  # consumers ignore it. Identity comes from `current_root`.
  ok                @0 :Bool;
  generation        @1 :UInt64;
  arenaPath         @2 :Text  $Json.name("arena_path");
  arenaSize         @3 :UInt64  $Json.name("arena_size");
  phase             @4 :Text;
  currentRoot       @5 :Text  $Json.name("current_root");
  enrichment        @6 :Text;
  # **Legacy** — JSON-encoded `{name → {last_run_at_ms?, basis?,
  # error?}}`. Pre-b0ea2e the canonical shape. Post-b0ea2e the daemon
  # leaves this field unset (capnp-json omits unset Text on the wire),
  # but the ordinal stays per ADR-0014 §2 in case any pinned
  # consumer still reads it. **Use `enrichmentTyped @10` instead.**
  headSha           @7 :Text  $Json.name("head_sha");
  lastReparseAtMs   @8 :Int64  $Json.name("last_reparse_at_ms");
  # On the Cap'n Proto side this field is always present (capnp ints
  # can't be absent and default to 0). The JSON wire emits "0" pre-
  # first-reparse since capnp-json has no skip-if-default annotation;
  # consumers ignore the value when "0" — see ADR-0014's interim
  # status section.
  error             @9 :Text;
  # Typed per-pass enrichment status — replaces the legacy
  # `enrichment @6 :Text` JSON-string field. List of `(name,
  # PassStatus)` entries, fully typed end-to-end. No double parse
  # on consumers. Added in b0ea2e (PR #12) after the Text field's
  # double-parse design flaw was caught in review.
  enrichmentTyped   @10 :List(EnrichmentEntry)  $Json.name("enrichment_typed");
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
  currentRoot  @3 :Text  $Json.name("current_root");
  parsed       @4 :UInt64;
  unchanged    @5 :UInt64;
  deleted      @6 :UInt64;
  errors       @7 :UInt64;
  changedFiles @8 :List(Text)  $Json.name("changed_files");
}

struct SnapshotResponse {
  ok          @0 :Bool;
  generation  @1 :UInt64;
  currentRoot @2 :Text  $Json.name("current_root");
}

struct FlushRequest {}

struct FlushResponse {
  ok          @0 :Bool;
  currentRoot @1 :Text  $Json.name("current_root");
}

struct EnrichRequest {
  pass  @0 :Text;
  files @1 :List(Text);
}

struct EnrichResponse {
  ok          @0 :Bool;
  generation  @1 :UInt64;
  passes      @2 :List(EnrichmentStats);
  currentRoot @3 :Text  $Json.name("current_root");
}

struct LoadRequest {
  db @0 :Data;   # raw .db bytes (not base64 — capnp handles binary)
}

struct LoadResponse {
  ok          @0 :Bool;
  generation  @1 :UInt64;
  currentRoot @2 :Text  $Json.name("current_root");
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
  nodeIds @1 :List(Text)  $Json.name("node_ids");
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
  dbPath       @1 :Text  $Json.name("db_path");
  ctrlPath     @2 :Text  $Json.name("ctrl_path");
  bindingsPath @3 :Text  $Json.name("bindings_path");
  astPath      @4 :Text  $Json.name("ast_path");
  sourcePath   @5 :Text  $Json.name("source_path");
  headPath     @6 :Text  $Json.name("head_path");
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
  headSeq      @1 :UInt64  $Json.name("head_seq");
  replayCount  @2 :UInt64  $Json.name("replay_count");
  replayGap    @3 :Bool  $Json.name("replay_gap");
}

struct ErrorResponse {
  error @0 :Text;
  ok    @1 :Bool;
  # On the JSON wire the error envelope emits `{"ok": false, "error": "..."}`.
  # Bool defaults to false in capnp; handlers never explicitly set this,
  # so the wire always reads `"ok": false` for error responses. Added
  # additively per ADR-0014 §2; pre-b0ea2e the field was on every typed
  # envelope via the hand-written wire.rs ErrorResponse struct.
}

# ── Sheaf cache UDS operations ─────────────────────────────────────
# Surfaces the leyline-sheaf SheafCache + CoChangeTracker over the
# daemon's UDS + MCP wire. Consumers (mache, cloister) push community
# topology and query structural cache invalidation.
#
# Six ops; all structs additive per ADR-0014 §2.

# Region stalk = content-hash summary for one cache region. The
# `hash` field is hex-encoded over the wire so consumers don't have
# to JSON-encode a 32-byte byte array.
#
# `data` is optional: when non-empty AND the request's `nodeStalkDim`
# is > 0, the cache pushes it into the attached `CellComplex` via
# `set_stalk_value` so `detect_violations` sees the latest section
# and the δ⁰-driven invalidation path engages. When empty, the
# region only feeds the XOR-Merkle heuristic.
struct SheafStalk {
  id   @0 :UInt32;
  hash @1 :Text;
  data @2 :List(Float32);
}

# Restriction edge between two cache regions. `boundaryHash` is the
# XOR-pre-filter hash; `weights` carries per-dimension learned
# coupling strengths (empty = `[1.0]` default in handler).
#
# `agreementDim` opts the edge into δ⁰-driven mode: the implicit
# restriction map is "project the first `agreementDim` coordinates"
# of each endpoint's stalk. When 0, only the XOR pre-filter governs
# this edge's cascade.
struct SheafRestriction {
  a            @0 :UInt32;
  b            @1 :UInt32;
  boundaryHash @2 :Text     $Json.name("boundary_hash");
  coChangeRate @3 :Float64  $Json.name("co_change_rate");
  revertRate   @4 :Float64  $Json.name("revert_rate");
  weights      @5 :List(Float64);
  agreementDim @6 :UInt32   $Json.name("agreement_dim");
}

struct SheafSetTopologyRequest {
  regions      @0 :List(SheafStalk);
  restrictions @1 :List(SheafRestriction);
  # Stalk dimension for δ⁰ mode. When > 0, every region's `data`
  # field must be exactly this length and every restriction's
  # `agreementDim` must be ≤ `nodeStalkDim`. The handler then
  # builds a backing `CellComplex` and runs `refresh_baseline()`
  # before returning so subsequent `sheaf_invalidate` calls land
  # against a snapshot of the seed state.
  nodeStalkDim @2 :UInt32  $Json.name("node_stalk_dim");
}

struct SheafSetTopologyResponse {
  ok            @0 :Bool;
  regions       @1 :UInt32;
  restrictions  @2 :UInt32;
  # Whether δ⁰-driven invalidation was activated (every stalk has
  # `data` of length `nodeStalkDim`, every restriction has
  # `agreementDim > 0`). False = the cache fell back to the XOR-
  # only heuristic path for this topology.
  deltaZeroMode @3 :Bool  $Json.name("delta_zero_mode");
}

struct SheafInvalidateRequest {
  regions @0 :List(UInt32);
  # Optional new stalks delivered alongside the invalidation hint.
  # When present, the handler updates the cache's stored stalks
  # before running on_change so the boundary check sees the new
  # state.
  stalks  @1 :List(SheafStalk);
}

struct SheafInvalidateResponse {
  invalidated @0 :List(UInt32);
  count       @1 :UInt32;
  generation  @2 :UInt64;
}

struct SheafDefectResponse {
  defect     @0 :Float64;
  generation @1 :UInt64;
  valid      @2 :UInt32;
  total      @3 :UInt32;
}

struct SheafStalksResponse {
  generation @0 :UInt64;
  valid      @1 :UInt32;
  total      @2 :UInt32;
}

struct SheafStatusResponse {
  generation   @0 :UInt64;
  valid        @1 :UInt32;
  total        @2 :UInt32;
  defect       @3 :Float64;
  trackedEdges @4 :UInt32  $Json.name("tracked_edges");
}

struct SheafLearnedWeight {
  a            @0 :UInt32;
  b            @1 :UInt32;
  coChangeRate @2 :Float64  $Json.name("co_change_rate");
  observations @3 :UInt64;
}

struct SheafLearnedWeightsResponse {
  ok        @0 :Bool;
  weights   @1 :List(SheafLearnedWeight);
  edgeCount @2 :UInt32  $Json.name("edge_count");
}

# ── Incremental topology updates ───────────────────────────────────
# `sheaf_update_topology` mirrors `sheaf_set_topology` but applies a
# delta instead of replacing the complex wholesale. After applying the
# delta the handler re-snapshots `‖δ⁰‖²` only for the touched subgraph
# (touched regions + radius-1 neighbours) and returns that affected
# region list — consumers use it as the eviction set, leaving cache
# entries for untouched regions byte-identical.
#
# Region IDs stay `UInt32` to match `SheafStalk.id` and
# `SheafRestriction.{a,b}`; introducing `Text` IDs here would split the
# wire's region-naming convention across two ops.

struct EdgeRef {
  source @0 :UInt32;
  target @1 :UInt32;
}

struct StalkUpdate {
  regionId @0 :UInt32       $Json.name("region_id");
  # New stalk values for the region. Must match the topology's
  # `node_stalk_dim` when δ⁰ mode is active.
  stalk    @1 :List(Float32);
}

struct TopologyDelta {
  addedRegions   @0 :List(SheafStalk)         $Json.name("added_regions");
  removedRegions @1 :List(UInt32)             $Json.name("removed_regions");
  addedEdges     @2 :List(SheafRestriction)   $Json.name("added_edges");
  removedEdges   @3 :List(EdgeRef)            $Json.name("removed_edges");
  updatedStalks  @4 :List(StalkUpdate)        $Json.name("updated_stalks");
}

struct SheafUpdateTopologyRequest {
  delta        @0 :TopologyDelta;
  # Matches the `node_stalk_dim` of the prior `sheaf_set_topology` call
  # when δ⁰ mode is active. Required for `addedRegions` whose `data`
  # must be exactly this length.
  nodeStalkDim @1 :UInt32  $Json.name("node_stalk_dim");
}

struct SheafUpdateTopologyResponse {
  ok              @0 :Bool;
  generation      @1 :UInt64;
  # Touched regions ∪ their radius-1 BFS neighbours. The consumer's
  # cache-eviction set — every region outside this list is guaranteed
  # to be byte-identical to its pre-update entry.
  affectedRegions @2 :List(UInt32)  $Json.name("affected_regions");
  # `Σ‖δ⁰‖²` snapshot AFTER the delta applies and the affected subgraph
  # baseline is refreshed. Lets consumers track sheaf health across
  # incremental updates without a separate `sheaf_defect` round-trip.
  defectAfter     @3 :Float32       $Json.name("defect_after");
}
