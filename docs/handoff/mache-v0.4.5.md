# Mache handoff — ley-line-open v0.4.5

**Status:** all four LLO PRs from the mache-bead campaign merged.
**Tags pushed:**

- `v0.4.5` — Rust daemon binary (`72f4f08e0a96...` was the merge commit; `git show v0.4.5` for the precise SHA)
- `clients/go/leyline-schema/v0.4.5` — Go schema-client module, on the same merge commit

**For consumers:** this single document is the hand-off contract. Read once, no other doc is required to consume v0.4.5.

---

## What changed since v0.4.4

Four PRs in the campaign, one release PR rolling them up:

| LLO PR | Bead | Title |
|---|---|---|
| #38 | `ley-line-open-503971` | typed JSON event payload structs (Go schema-client) |
| #39 | `ley-line-open-cb8960` | `leyline_version` handshake op |
| #40 | `ley-line-open-cbea02` | self-maintaining `compatibility.json` |
| #41 | — | HCL / Terraform parse via `tree-sitter-hcl` |
| #42 | — | release v0.4.5 |

All four landed on the same tag. The first three are mutually reinforcing — the handshake op surfaces the same constants the compat artifact derives from; typed event payloads eliminate the silent-coercion bug class the handshake is designed to detect at connect time.

---

## 1. Bump the schema-client pin

In mache's `go.mod`:

```diff
- github.com/agentic-research/ley-line-open/clients/go/leyline-schema v0.4.4
+ github.com/agentic-research/ley-line-open/clients/go/leyline-schema v0.4.5
```

Then `go mod tidy`. The new sub-package `clients/go/leyline-schema/daemon/wire` becomes importable.

---

## 2. Adopt the typed event payloads (bead `ley-line-open-503971`)

### What's new

- New sub-package `github.com/agentic-research/ley-line-open/clients/go/leyline-schema/daemon/wire`.
- Op-response types (`StatusResponse`, `SheafInvalidateResponse`, `SheafLearnedWeightsResponse`, etc.) — promoted from the test-internal lowercase types that used to live in `daemon/daemon_protocol_test.go`. All exported, all u64/i64 fields tagged `json:",string"` per the capnp_json quoted-string convention.
- Event payload types — every topic the daemon emits gets a typed struct:
  - `Event` (the envelope: `event` discriminator + `seq` + `topic` + `source` + `data`)
  - `SheafInvalidatePayload`
  - `SheafTopologyPayload` (one struct covers both `op_sheaf_set_topology` emits and `op_sheaf_update_topology` emits; use the `Kind` field as discriminator — `"update"` for incremental, nil/empty for set)
  - `DaemonSnapshotPayload`, `DaemonHeadChangedPayload`, `DaemonFilesChangedPayload`, `DaemonReparseCompletePayload`, `DaemonOpPayload`
- `DecodeEvent(b []byte) (Event, any, error)` — one call to dispatch + decode; returns the envelope plus the typed payload for known topics and `json.RawMessage` for unknown topics (forward-compat: future LLO topics don't break v0.4.5 consumers).

### What to delete in mache

The paired mache bead `mache-5159a2` lists the hand-rolled parsers that v0.4.5 makes obsolete:

- `parseUint64` (sheaf.go:438) — silently returned 0 on malformed input. With typed `json.Unmarshal` + `,string` tags, malformed input becomes a typed error.
- `parseIntSlice` (sheaf.go:420) — `[]any` → `[]int` via float64 type-switch.
- The `map[string]any` indexing in `sheaf_subscriber.go::dispatch` and `sheaf.go::Status`.

### Recommended pattern

```go
import "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/daemon/wire"

// SheafSubscriber's dispatch path — replaces the map[string]any block.
for line := range c.eventCh {
    ev, payload, err := wire.DecodeEvent(line)
    if err != nil {
        log.Warnf("ley-line-open decode: %v", err)
        continue
    }
    switch p := payload.(type) {
    case wire.SheafInvalidatePayload:
        h.OnSheafInvalidate(ev, p) // p.Generation is *uint64, p.Invalidated is []uint32
    case wire.SheafTopologyPayload:
        if p.Kind != nil && *p.Kind == "update" {
            h.OnSheafTopologyUpdate(ev, p)
        } else {
            h.OnSheafTopologySet(ev, p)
        }
    case wire.DaemonReparseCompletePayload:
        h.OnReparse(ev, p)
    // ... etc
    default:
        // json.RawMessage — unknown topic, forward or log
    }
}
```

### Wire-shape caveat to remember

`Generation` and `PriorGeneration` are quoted-string JSON values on the wire (capnp_json u64 convention). The `,string` tag handles this transparently — your Go code sees `*uint64`, the JSON wire carries `"1"` not `1`. This was the v0.4.3 wire-shape change that motivated the typed-payload bead in the first place.

---

## 3. Adopt the handshake op (bead `ley-line-open-cb8960`)

### What's new

New op `{"op":"leyline_version"}` returns the daemon's runtime identity:

```json
{
  "ok": true,
  "binary_version": "0.4.5",
  "schema_version": "0.4.5",
  "wire_format_major": 1,
  "compat_min": "0.4.1",
  "build_date": "unspecified"
}
```

### When to call

Immediately after `DialSocket`, before any other op. Fail loudly on incompatibility rather than discovering it via silent parser drift downstream.

### Recommended pattern

```go
// In SocketClient.Dial or wherever the first round-trip happens.
ver, err := c.SendOp(map[string]any{"op": "leyline_version"})
if err != nil {
    return nil, fmt.Errorf("leyline_version handshake: %w", err)
}
// Decode into a typed struct if you want; or just check the fields.
binV, _ := ver["binary_version"].(string)
wireMaj, _ := ver["wire_format_major"].(float64) // JSON number, NOT stringified
compatMin, _ := ver["compat_min"].(string)

const MacheMinDaemon = "0.4.1" // raise floor in mache when needed
if semver.Compare("v"+binV, "v"+MacheMinDaemon) < 0 {
    return nil, fmt.Errorf(
        "ley-line-open v%s too old for this mache build (needs >= v%s); see %s",
        binV, MacheMinDaemon, compatMin,
    )
}
```

### Pairs with `mache-8kif`

The mache-side startup version check bead. The handshake's daemon-side response is what `mache-8kif` consumes. If you have `mache-8kif` in flight, it should consume this directly.

---

## 4. Compatibility artifact (bead `ley-line-open-cbea02`)

For build-time consumers (e.g., a mache build script that wants to know "does my pinned daemon version still match my schema-client?") without a live daemon, the same constants are baked into a static artifact:

- `compatibility.json` at the LLO repo root. Same fields as the handshake op plus a `$schema_version: 1` doc-version tag.
- Generated by `task compat:gen` from `rs/ll-open/cli-lib/src/daemon/version.rs`. CI-gated against drift.
- Available at every release tag — fetch from `https://raw.githubusercontent.com/agentic-research/ley-line-open/v0.4.5/compatibility.json` for a stable URL per release.

If mache wants to verify compatibility at build time (instead of/in addition to the handshake at run time), point at this URL with the desired tag and gate the build.

---

## 5. HCL / Terraform parse support (PR #41)

### What's new

The daemon now parses `.tf`, `.tfvars`, and `.hcl` files via `tree-sitter-hcl`. No mache wiring needed — the daemon's TreeSitter pass picks them up automatically.

### Surface for mache

- `find_callers` / `find_defs` / `find_callees` / `get_node` / `read_content` work on Terraform symbols the same way they work on Go / Python / Rust symbols.
- Language-tag aliases on `--lang` flag: `hcl`, `terraform`, `tf`, `tfvars` — all four resolve to the same backend.
- Mache's existing language-tag passthrough should just work; no change needed if mache reads tags from the daemon.

### Validation suggestion

Point a daemon at a Terraform tree (`leyline daemon --source path/to/terraform/`), then issue `find_callers` for a known resource name. The same call shape that works for Go works for Terraform.

---

## End-to-end validation checklist

These items are the e2e validation gap LLO can't close without mache. None are blocking, but they're the proof points before mache merges PR #384:

- [ ] Bump `clients/go/leyline-schema` to v0.4.5 in mache's `go.mod`.
- [ ] Build mache against v0.4.5; verify no compile breakage from the new `wire/` package import path.
- [ ] Call `leyline_version` once at connect from `SocketClient` (or wherever mache's daemon-spawn flow lives) and log the response.
- [ ] Run `TestE2E_SheafSubscriber_AgainstLiveDaemon` against a v0.4.5 daemon binary; verify it flips RED→GREEN with no mache-side code change other than the version pin.
- [ ] Optional: parse a small `.tf` file via mache's existing query surface; confirm the daemon serves the same op shapes as it does for Go/Rust.
- [ ] Optional: delete `parseUint64` / `parseIntSlice` / `map[string]any` indexing per `mache-5159a2`; replace with the typed `wire.DecodeEvent` pattern.

---

## What this hand-off does NOT cover

- **Substrate beads filed but not implemented:** `ley-line-open-79a37c` (session ingest), `ley-line-open-79c6ab` (codex+slack source impls), `ley-line-open-79fd04` (chat-embed refactor), `ley-line-open-783d72` (BlobStore trait). These are the LLO-side substrate work queued behind ADR-0020 (which is merged but hasn't moved Proposed → Accepted yet). Not in v0.4.5.
- **Cross-machine federation:** any `org:user:` token sharing across machines. The current substrate is per-machine.
- **Lens-style aggregations** (`lens.developer` / `lens.em` / `lens.code_archaeology` from ADR-0020). The ADR explicitly defers lenses to "compositions emerge as needed"; v0.4.5 ships none.

These are real future work tracked in separate beads. Mache shouldn't expect any of them in v0.4.5; they'll show up in a later cycle if and when a consumer pressure-tests the need.

---

## Anchor links

- LLO release: https://github.com/agentic-research/ley-line-open/releases/tag/v0.4.5
- Go module tag: https://github.com/agentic-research/ley-line-open/tree/clients/go/leyline-schema/v0.4.5
- Compatibility artifact: https://raw.githubusercontent.com/agentic-research/ley-line-open/v0.4.5/compatibility.json
- ADR-0020 (substrate direction): `docs/adr/0020-entity-observation-lattice.md`
- Bead corpus (rosary): `rosary-7023a9` (observation-lattice parent), `rosary-125fc1` (chat-log capture), `rosary-cdaa16` (capnp issue-type loader precedent)
