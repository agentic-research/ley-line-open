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
	CurrentRoot *string `json:"current_root,omitempty"`
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
	// SourceRoot is the daemon's `--source` directory with its symlinks
	// resolved: the tree a [ProposedWrite.Path] and a `read_source` path
	// are relative to. Nil when the daemon runs without `--source`, which
	// leaves a client no tree to propose into. Bead ley-line-open-246526.
	SourceRoot *string `json:"source_root,omitempty"`
	// RootPublished says whether the controller names a published arena
	// root, and is always present. The root absence rule (daemon.capnp,
	// StatusResponse.rootPublished; bead ley-line-open-80f037): CurrentRoot
	// is non-nil exactly when RootPublished is true. The daemon never sends
	// a zero or empty current_root.
	RootPublished *bool `json:"root_published"`
}

// SnapshotResponse is the wire shape of the `snapshot` op response.
// CurrentRoot follows the root absence rule on
// [StatusResponse.RootPublished].
type SnapshotResponse struct {
	OK            *bool   `json:"ok"`
	Generation    *uint64 `json:"generation,string"`
	CurrentRoot   *string `json:"current_root,omitempty"`
	RootPublished *bool   `json:"root_published"`
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

// ── read_source / propose (ADR-0042, ADR-0045; bead ley-line-open-247489) ──

// ReadSourceResponse is the wire shape of the `read_source` op response:
// one whole file by its path relative to the tree root, with the blob hash
// a later [ProposedWrite.Base] names.
type ReadSourceResponse struct {
	OK *bool `json:"ok"`
	// Content is the file's bytes as UTF-8, or a symlink's target.
	Content *string `json:"content,omitempty"`
	// BlobHash is the file's `_tree.blob_hash`, 64 hex chars.
	BlobHash *string `json:"blob_hash,omitempty"`
	// TreeRoot is the arena's treeRoot the file was read under, 64 hex chars.
	TreeRoot *string `json:"tree_root,omitempty"`
	Error    *string `json:"error,omitempty"`
	// Mode is the file's git mode, six octal digits: [ModeRegular],
	// [ModeExecutable] or [ModeSymlink].
	Mode *string `json:"mode,omitempty"`
}

// Git modes a [ReadSourceResponse.Mode] reports and a [ProposedWrite.Mode]
// may name (ADR-0045).
const (
	ModeRegular    = "100644"
	ModeExecutable = "100755"
	ModeSymlink    = "120000"
)

// Base spellings a [ProposedWrite.Base] may carry besides a 64-hex-char
// blob hash.
const (
	// BaseAny writes whatever the path holds.
	BaseAny = "any"
	// BaseAbsent names no file: a create's or mkdir's base, and the
	// [StaleRefusal.Expected] or [StaleRefusal.Current] side that is no file.
	BaseAbsent = "absent"
)

// WriteKind is what one [ProposedWrite] does at its path, in its wire
// spelling (the capnp `WriteKind` enumerant name).
type WriteKind string

// The WriteKind enumerants, in capnp ordinal order.
const (
	WriteKindReplace WriteKind = "replace"
	WriteKindCreate  WriteKind = "create"
	WriteKindDelete  WriteKind = "delete"
	WriteKindMkdir   WriteKind = "mkdir"
	WriteKindRmdir   WriteKind = "rmdir"
)

// ProposedWrite is one path of a `propose` request's `writes` list; the
// request is `{"op":"propose","writes":[...]}`. Path is relative to the
// daemon's tree ([StatusResponse.SourceRoot] for a daemon with `--source`).
// Base is required: a [ReadSourceResponse.BlobHash], [BaseAny], or
// [BaseAbsent]. Content is required by a replace and a create (a pointer to
// "" writes an empty file) and ignored otherwise. An empty Kind is omitted
// and the daemon reads it as [WriteKindReplace]. A nil Mode keeps the
// file's mode (a create without one is [ModeRegular]).
type ProposedWrite struct {
	Path    string    `json:"path"`
	Base    string    `json:"base"`
	Content *string   `json:"content,omitempty"`
	Kind    WriteKind `json:"kind,omitempty"`
	Mode    *string   `json:"mode,omitempty"`
}

// PathHash is one written path's new blob hash in [ProposeResponse.Blobs].
type PathHash struct {
	Path     *string `json:"path"`
	BlobHash *string `json:"blob_hash"`
}

// LineRange is a range of a base's lines, 0-based and half-open:
// `[Start, End)`. Two insertions at one line are the empty range
// `[line, line)`.
type LineRange struct {
	Start *uint32 `json:"start"`
	End   *uint32 `json:"end"`
}

// StaleRefusal is a path that moved since its author read it. Expected or
// Current is [BaseAbsent] when that side is no file.
type StaleRefusal struct {
	Path     *string `json:"path"`
	Expected *string `json:"expected"`
	Current  *string `json:"current"`
	// Conflicts is set when the write was merged three ways and both sides
	// changed overlapping lines: the lines of Expected where they overlap.
	// Empty when the path could not be merged at all.
	Conflicts []LineRange `json:"conflicts,omitempty"`
}

// RefusalReason is why a proposal was refused, in its wire spelling (the
// capnp `RefusalReason` enumerant's `$Json.name`). A reason this package
// does not name still decodes; a client must treat it as a refusal.
type RefusalReason string

// The RefusalReason enumerants, in capnp ordinal order. The `Retired`
// reasons are never sent; their ordinals are kept (ADR-0045).
const (
	RefusalStale               RefusalReason = "stale"
	RefusalOutsideGrant        RefusalReason = "outside_grant"
	RefusalNoSuchFile          RefusalReason = "no_such_file"
	RefusalDuplicatePath       RefusalReason = "duplicate_path"
	RefusalDifferentRoots      RefusalReason = "different_roots"
	RefusalNoTreeRoot          RefusalReason = "no_tree_root"
	RefusalRetiredSyntax       RefusalReason = "retired_syntax"
	RefusalMalformedBase       RefusalReason = "malformed_base"
	RefusalRetiredNoStoredBlob RefusalReason = "retired_no_stored_blob"
	RefusalEmpty               RefusalReason = "empty"
	RefusalRetiredNoLanguage   RefusalReason = "retired_no_language"
	RefusalMalformedPath       RefusalReason = "malformed_path"
	RefusalPathConflict        RefusalReason = "path_conflict"
	RefusalExists              RefusalReason = "exists"
	RefusalNotEmpty            RefusalReason = "not_empty"
	RefusalNotADirectory       RefusalReason = "not_a_directory"
	RefusalIsADirectory        RefusalReason = "is_a_directory"
	// RefusalAliasesExisting: a created path names, in the daemon's
	// checkout, the entry [ProposalRefusal.Other]; the checkout's file
	// system folds the two spellings onto one file.
	RefusalAliasesExisting RefusalReason = "aliases_existing"
)

// ProposalRefusal is why a proposal was refused. Nothing was written.
type ProposalRefusal struct {
	Reason *RefusalReason `json:"reason"`
	// Path is the file the refusal names; nil for stale and empty.
	Path *string `json:"path,omitempty"`
	// Moved is, for stale only, every file that moved.
	Moved []StaleRefusal `json:"moved,omitempty"`
	// Other is the second path the refusal names: for path_conflict the
	// file Path is under, for aliases_existing the entry Path aliases.
	Other *string `json:"other,omitempty"`
}

// ProposeResponse is the wire shape of the `propose` op response.
//
// OK true means the proposal was applied. OK false with Refusal means it
// was refused and nothing changed; OK false without Refusal is an internal
// error. A refused response also carries Error, a human-readable message
// beside Refusal: decide on OK and Refusal, never on whether Error is set.
type ProposeResponse struct {
	OK *bool `json:"ok"`
	// TreeRoot is the arena's treeRoot after the proposal (OK only).
	TreeRoot *string `json:"tree_root,omitempty"`
	// Blobs is each written path's new blob hash (OK only).
	Blobs   []PathHash       `json:"blobs,omitempty"`
	Refusal *ProposalRefusal `json:"refusal,omitempty"`
	Error   *string          `json:"error,omitempty"`
	// PostCommitError is, OK only, a step after the commit that failed. The
	// proposal is applied.
	PostCommitError *string `json:"post_commit_error,omitempty"`
	// Merged is, OK only, the written paths whose base had moved and whose
	// content is the three-way merge with what the path held.
	Merged []string `json:"merged,omitempty"`
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
