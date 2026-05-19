// Package daemon provides typed Go bindings for the ley-line-open daemon's
// JSON wire surface — both per-op responses and the pushed-event payload
// shapes that ride inside the capnp `Event.data` field.
//
// These structs were originally unexported and lived in `daemon_protocol_test.go`
// where they backed the cross-runtime fixture suite (ADR-0014 §F8.6.4). They
// are promoted here so external consumers (mache, cloister, future LLO
// clients) can `json.Unmarshal` daemon wire payloads with type safety
// instead of hand-rolled `map[string]any` indexing.
//
// # Quoted-string u64 encoding
//
// The `json:",string"` tag on every `uint64` / `int64` field is the
// load-bearing convention from capnp_json: u64 fields are emitted as
// JSON *strings* on the wire to dodge JS Number's 2^53 safe-integer
// ceiling. A consumer that decodes these as raw numbers (without the
// `,string` tag) parses a malformed value silently as 0. The bead
// `ley-line-open-503971` (commit history of this file) names the
// `parseUint64` silent-coercion bug class this typing eliminates.
//
// # Optional vs required
//
// Every field is a pointer (`*T`) so `json.Unmarshal` can distinguish
// "field absent" from "field set to zero value." This is the same
// discipline the original test-internal types used. Consumers that
// know a field is always present can dereference; consumers that don't
// can nil-check.

package wire

// PassStatus is the per-enrichment-pass status entry under
// [StatusResponse.EnrichmentTyped].
type PassStatus struct {
	LastRunAtMs *int64  `json:"last_run_at_ms,string"`
	Basis       *uint64 `json:"basis,string"`
	Error       *string `json:"error,omitempty"`
}

// EnrichmentEntry is one row in the typed enrichment-pass status list.
type EnrichmentEntry struct {
	Name   *string     `json:"name"`
	Status *PassStatus `json:"status"`
}

// StatusResponse is the wire shape of the `status` op response.
type StatusResponse struct {
	OK          *bool   `json:"ok"`
	Generation  *uint64 `json:"generation,string"`
	ArenaPath   *string `json:"arena_path"`
	ArenaSize   *uint64 `json:"arena_size,string"`
	Phase       *string `json:"phase"`
	CurrentRoot *string `json:"current_root"`
	// Enrichment is the legacy JSON-encoded enrichment-status string
	// (pre-b0ea2e). The daemon no longer emits it as of v0.4.x; the
	// typed shape lives in [StatusResponse.EnrichmentTyped]. Kept here
	// as `omitempty` for back-compat with consumers still pinned to
	// the v0.2.x bindings. New consumers should use EnrichmentTyped.
	Enrichment      *string           `json:"enrichment,omitempty"`
	EnrichmentTyped []EnrichmentEntry `json:"enrichment_typed"`
	HeadSHA         *string           `json:"head_sha,omitempty"`
	LastReparseAtMs *int64            `json:"last_reparse_at_ms,string"`
	Error           *string           `json:"error,omitempty"`
}

// FlushResponse is the wire shape of the `flush` op response.
type FlushResponse struct {
	OK          *bool   `json:"ok"`
	CurrentRoot *string `json:"current_root"`
}

// SnapshotResponse is the wire shape of the `snapshot` op response.
type SnapshotResponse struct {
	OK          *bool   `json:"ok"`
	Generation  *uint64 `json:"generation,string"`
	CurrentRoot *string `json:"current_root"`
}

// Node is one row in the daemon's tree / per-node responses.
type Node struct {
	ID       *string `json:"id"`
	ParentID *string `json:"parent_id"`
	Name     *string `json:"name"`
	Kind     *int32  `json:"kind"`
	Size     *int64  `json:"size,string"`
	// Record is omitted by list_children (directory listings stay small;
	// see Copilot review on PR #8). get_node / read_content emit it
	// when the SQL `record` column is non-null.
	Record *string `json:"record,omitempty"`
}

// Ref is one (node_id, source_id) pair returned by find_callers /
// find_defs / find_callees.
type Ref struct {
	NodeID   *string `json:"node_id"`
	SourceID *string `json:"source_id"`
}

// QueryRow is one row in a `query` op response.
type QueryRow struct {
	Values []string `json:"values"`
}

// ReadContentResponse is the wire shape of the `read_content` op response.
type ReadContentResponse struct {
	OK      *bool   `json:"ok"`
	Content *string `json:"content,omitempty"`
	Error   *string `json:"error,omitempty"`
}

// ListChildrenResponse is the wire shape of the `list_children` op response.
type ListChildrenResponse struct {
	OK       *bool  `json:"ok"`
	Children []Node `json:"children"`
}

// GetNodeResponse is the wire shape of the `get_node` op response.
type GetNodeResponse struct {
	OK    *bool   `json:"ok"`
	Node  *Node   `json:"node,omitempty"`
	Error *string `json:"error,omitempty"`
}

// FindCallersResponse is the wire shape of the `find_callers` op response.
type FindCallersResponse struct {
	OK      *bool `json:"ok"`
	Callers []Ref `json:"callers"`
}

// FindDefsResponse is the wire shape of the `find_defs` op response.
type FindDefsResponse struct {
	OK   *bool `json:"ok"`
	Defs []Ref `json:"defs"`
}

// FindCalleesResponse is the wire shape of the `find_callees` op response.
type FindCalleesResponse struct {
	OK      *bool `json:"ok"`
	Callees []Ref `json:"callees"`
}

// TokenMapEntry is one (token, node_ids) pair in a refs-map / defs-map response.
type TokenMapEntry struct {
	Token   *string  `json:"token"`
	NodeIDs []string `json:"node_ids"`
}

// GetRefsMapResponse is the wire shape of the `get_refs_map` op response.
type GetRefsMapResponse struct {
	OK      *bool           `json:"ok"`
	Entries []TokenMapEntry `json:"entries"`
}

// GetDefsMapResponse is the wire shape of the `get_defs_map` op response.
type GetDefsMapResponse struct {
	OK      *bool           `json:"ok"`
	Entries []TokenMapEntry `json:"entries"`
}

// SchemaTier is one entry in the `get_schema` op response.
type SchemaTier struct {
	Name   *string  `json:"name"`
	Crates []string `json:"crates"`
}

// GetSchemaResponse is the wire shape of the `get_schema` op response.
type GetSchemaResponse struct {
	OK    *bool        `json:"ok"`
	Tiers []SchemaTier `json:"tiers"`
}

// GetDbPathResponse is the wire shape of the `get_db_path` op response.
type GetDbPathResponse struct {
	OK           *bool   `json:"ok"`
	DBPath       *string `json:"db_path"`
	CtrlPath     *string `json:"ctrl_path"`
	BindingsPath *string `json:"bindings_path"`
	ASTPath      *string `json:"ast_path"`
	SourcePath   *string `json:"source_path"`
	HeadPath     *string `json:"head_path"`
}

// QueryResponse is the wire shape of the `query` op response.
type QueryResponse struct {
	OK      *bool      `json:"ok"`
	Columns []string   `json:"columns"`
	Rows    []QueryRow `json:"rows"`
}

// ── Sheaf op responses ─────────────────────────────────────────────

// SheafSetTopologyResponse is the wire shape of the `sheaf_set_topology`
// op response. DeltaZeroMode is true iff the request engaged δ⁰ mode
// (every region carrying f32 data of the declared dimension and every
// restriction having agreement_dim > 0).
type SheafSetTopologyResponse struct {
	OK            *bool   `json:"ok"`
	Regions       *uint32 `json:"regions"`
	Restrictions  *uint32 `json:"restrictions"`
	DeltaZeroMode *bool   `json:"delta_zero_mode"`
}

// SheafInvalidateResponse is the wire shape of the `sheaf_invalidate`
// op response. The `prior_generation` field was added in v0.4.1 (bead
// ley-line-open-9d5d7d) — it carries the generation value immediately
// before the op bumped it, so consumers can verify
// `their_last_seen == response.prior_generation` and detect missed
// events between two generations.
type SheafInvalidateResponse struct {
	Invalidated     []uint32 `json:"invalidated"`
	Count           *uint32  `json:"count"`
	Generation      *uint64  `json:"generation,string"`
	PriorGeneration *uint64  `json:"prior_generation,string"`
}

// SheafDefectResponse is the wire shape of the `sheaf_defect` op response.
type SheafDefectResponse struct {
	Defect     *float64 `json:"defect"`
	Generation *uint64  `json:"generation,string"`
	Valid      *uint32  `json:"valid"`
	Total      *uint32  `json:"total"`
}

// SheafStalksResponse is the wire shape of the `sheaf_stalks` op response.
type SheafStalksResponse struct {
	Generation *uint64 `json:"generation,string"`
	Valid      *uint32 `json:"valid"`
	Total      *uint32 `json:"total"`
}

// SheafStatusResponse is the wire shape of the `sheaf_status` op response.
type SheafStatusResponse struct {
	Generation   *uint64  `json:"generation,string"`
	Valid        *uint32  `json:"valid"`
	Total        *uint32  `json:"total"`
	Defect       *float64 `json:"defect"`
	TrackedEdges *uint32  `json:"tracked_edges"`
}

// SheafLearnedWeight is one row in [SheafLearnedWeightsResponse.Weights].
type SheafLearnedWeight struct {
	A            *uint32  `json:"a"`
	B            *uint32  `json:"b"`
	CoChangeRate *float64 `json:"co_change_rate"`
	Observations *uint64  `json:"observations,string"`
}

// SheafLearnedWeightsResponse is the wire shape of the
// `sheaf_learned_weights` op response.
type SheafLearnedWeightsResponse struct {
	OK        *bool                `json:"ok"`
	Weights   []SheafLearnedWeight `json:"weights"`
	EdgeCount *uint32              `json:"edge_count"`
}
