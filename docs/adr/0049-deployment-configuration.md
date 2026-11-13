# ADR-0049 — Deployment configuration: one versioned document per daemon kind

**Status:** Accepted (owner, 2026-11-12), not yet implemented. Nothing in "Decisions" ships;
implementation is v0.30.0. The owner's answers are recorded under "Decided questions", and the
cloister and mache consumer reviews of the proposed draft (`06cf7501`) are folded in.
**Bead:** `ley-line-open-52c177`
**Consumers:** cloister `cloister-55e6cd` (emits `ExecutionDaemonConfig` and `DaemonConfig`
from `cluster.toml`, or resolves `ExecutionDaemonConfig` in-process through the library entry,
D13), cloister `cloister-0bc3d5` (runs `leyline-sign-helper` from a `SignHelperConfig`), mache
`mache-0be425` (the managed daemon inherits removed variables from the user's environment).
See D12.
**Related:** ADR-0019 (sign-only trust-anchor helper), ADR-0022 (MCP shared-secret token gate),
ADR-0035 (policy in cloister, mechanism in LLO), ADR-0044 (workspace shares, read freshness),
ADR-0046 (`connections/v1`, whose canonical-JSON rules D6 reuses), `schema-spec/LAYOUT.md`
("the IDL format follows the digest definition").

Tree cited throughout: `origin/integration` at `e6142dd4`.

---

## Decided questions (owner, 2026-11-12)

- **Q1 — Discriminator:** `apiVersion` + `kind` (D1).
- **Q2 — Precedence:** the document or flags, never both (D3). Flags stay as an adapter.
- **Q3 — `mount` in release binaries:** yes, in v0.30.0; the release-gate and asset changes are
  in the implementation slice. `fuse` stays opt-in (it links libfuse).
- **Q4 — `mcp.groups` default:** delegated to this ADR; the default stays "every group" (D5,
  "Q4"), for the reason recorded there.
- **Q5 — Sign helper:** deny by default: `auth.required` and `signAllow.required` default to
  `true` (D5).
- **Q6 — `configDigest` on the wire:** yes. It goes in the daemon's `status` reply, the
  `RunReceipt` (`@17`) and the in-process `RunOutcome` (D6, "Where `configDigest` appears").

## Context: what ships today

LLO's long-running processes are configured three ways, none of which can be validated before
start or printed afterwards:

1. **clap flags.** `leyline execution-daemon` has 23 (`rs/ll-open/cli/src/main.rs:72-209`,
   three of them flattened from `WorkspaceFreshness`, `main.rs:24-35`), `leyline daemon` 18
   (`main.rs:212-349`) and `leyline-sign-helper` 5 (`rs/ll-open/sign/src/bin/helper.rs:55-90`).
   Relations between flags are partly clap attributes (`requires`, `conflicts_with`) and partly
   hand-written checks scattered through `main` (`--device` needs `--backend micro-vm`,
   `main.rs:561`; `--cgroup-root` needs `--backend native`, `main.rs:608`; a non-loopback
   `--mcp-bind` needs `--mcp-allow-public`, `cmd_daemon.rs:356-372`).
2. **Environment variables, read ad hoc deep in the call graph.** 16 `LEYLINE_*` and 7 `LLO_*`
   variables change runtime behaviour, plus `KEYCHAIN_ACCOUNT` (inventory in "Migration
   table"). Two of them carry secret values inline: `LEYLINE_HEAD_SIGNING_KEY` is an Ed25519
   seed in hex (`cli-lib/src/cmd_parse.rs:3016`), and `LEYLINE_SIGN_CALLER_TOKENS` holds bearer
   tokens as `name=token,...` (`sign/src/bin/helper.rs:143`). Several change the parser
   identity in a release binary: `LLO_TRUSTED_QUERY_HASHES` (`cmd_parse.rs:2900`),
   `LLO_EXTRACTION_EPOCH` (`ts/src/refs.rs:76`) and `LLO_INJECTIONS_SCM`
   (`ts/src/injections.rs:233`) all move `Head.parserId`.
3. **Implicit host paths.** `~/.mache/default.arena` and `~/.mache/execution.arena`
   (`main.rs:536-542`, `696-700`), the MCP token at the platform data directory
   (`cli-lib/src/daemon/auth.rs:46`), the state directory (`runtime/src/cas_volume.rs:139`).

The trusted catalog (`--catalog`) is the only document a deployment supplies today.

Two consumers cannot be served by this. **Cloister** assembles argv and environment for LLO,
and has no way to check what it assembled except by starting the daemon. **A standalone
user** who wants the leyline-fs mount has to discover ~20 flags, an environment variable
family, and that release binaries are built without the `mount` feature
(`.github/workflows/release.yml:16-17`), so a release download refuses `--mount`.

## Thesis

Each LLO daemon kind is configured by one typed, versioned JSON document. JSON is the native
form, validated by a published JSON Schema; every other syntax is an adapter that produces the
same JSON value (Caddy's Caddyfile-to-JSON model). Every field has an explicit default stated in
the schema, so an omitted field means LLO's default, never "unset". The binary can validate a
document without starting (`--check-config`) and print the fully resolved document with every
default filled in (`--print-config`), so a consumer that writes only the fields it has
declarations for can test against exactly what will run. The document configures mechanism; it
carries no per-run authority (ADR-0035).

## Decisions (accepted, not yet implemented)

### D1 — Kinds: one document per daemon kind, one document per file

Three kinds, one per long-running process that has flags. Every document begins with the
Kubernetes ComponentConfig pair (owner, Q1):

```json
{"apiVersion": "config.leyline/v1", "kind": "DaemonConfig", ...}
```

| `kind` (with `apiVersion` `config.leyline/v1`) | Process | Today's flags | Short name in this ADR |
|---|---|---|---|
| `DaemonConfig` | `leyline daemon` | 18 | `daemon-config` |
| `ExecutionDaemonConfig` | `leyline execution-daemon` | 23 | `execution-daemon-config` |
| `SignHelperConfig` | `leyline-sign-helper` | 5 | `sign-helper-config` |

**One file holds exactly one document.** A file with several documents (the Kubernetes `---`
stream) needs a selection rule, lets one process read another's secret references, and gives a
file no single digest. Each process reads one file, and cloister emits one file per placement.

**Not kinds:** the one-shot subcommands (`parse`, `load`, `cdc`, `inspect`, `verify-head`,
`rebuild`, `rootfs`, `splice`, `lsp`, `self`, `doctor` and `execution {capabilities, provision,
status, inspect, collect, cleanup, start, cancel}`, `cli-lib/src/lib.rs:225-443`). Their
arguments are a request, not a deployment, and they stay CLI-only. Three of them must agree
with a daemon about the parser and the head policy: `parse` signs and verifies heads and
computes `parserId`; `verify-head` and `rebuild` recompute `parserId` from the trusted query
hashes (`cmd_parse.rs:2895-2900`, `cmd_rebuild.rs:64`). They accept `--config <file>` naming a
`daemon-config` or `execution-daemon-config` document and read only its `parser`, `head` and
`diagnostics` sections. One must agree about the state directory: `rootfs provision-cas`
(`cmd_rootfs.rs:133`, `provision_cas`) accepts `--config` naming an `execution-daemon-config`
document and reads only `stateDir`, which only that kind has. Any other kind is refused by
name. The worker binaries
(`leyline-native-worker`, `leyline-krun-worker`) are spawned by the execution daemon with argv
it builds, and are not deployment-configured.

**Discriminator.** Two keys, checked before anything else and in this order: `apiVersion`
(absent `missing-key`, not a string `wrong-type`, any value other than `config.leyline/v1`
`bad-api-version`, naming the value found and the one this binary accepts), then `kind` (absent
`missing-key`, not a string `wrong-type`, a value that is no kind `unknown-kind`, and a real kind
for another process `wrong-kind`, naming both, so a `DaemonConfig` handed to the execution daemon
is refused by name). The major version lives in `apiVersion` and covers all three kinds, so a
v2 of one kind is a new `apiVersion` for that kind only (`config.leyline/v2` with the same
`kind`); the pair is the discriminator, and a pair this binary does not know is refused. This
departs from the single `version` string `connections/v1` uses, by the owner's choice (Q1):
the pair is the convention operators know from Kubernetes, and refusing every unknown pair
closes the "pair disagrees with itself" case the single string avoided.

### D2 — Schema home: JSON-first in schema-spec, with vectors

The normative shape is a hand-written JSON Schema (draft 2020-12), not a projection from Cap'n
Proto. `schema-spec/LAYOUT.md` decides this: the digest D6 defines is taken over canonical
JSON, so the JSON Schema is the source and Cap'n Proto would be a second source for the same
surface. The dialect follows what `capnpc-schema-bridge-jsonschema` already emits
(`rs/ll-open/schema-bridge/src/outputs/json_schema.rs`): draft 2020-12, `$defs`,
`additionalProperties: false` on every object, single-key objects for unions (every union in v1
is one, `evidence` included: `{"unverified": {}}` or `{"verified": {...}}`). That keeps one
JSON Schema dialect across LLO's published schemas.

```
rs/ll-core/schema-spec/deployment-config/v1/
├── README.md                          the spec: fields, rules, refusal codes, canonical form
├── common.schema.json                 $defs shared by the kinds: path, secretRef, parser, head, diagnostics
├── daemon-config.schema.json
├── execution-daemon-config.schema.json
├── sign-helper-config.schema.json
├── test-vectors/
│   ├── daemon-config-cases.json       accepted inputs with resolved bytes + configDigest per platform; rejects with field + reason
│   ├── execution-daemon-config-cases.json
│   ├── sign-helper-config-cases.json
│   ├── canonical-strings.json         non-ASCII paths, astral-plane characters, empty lists and objects
│   └── toml-equivalence.json          TOML text → the same resolved bytes as its JSON twin
├── removed-variables.source.json     the D3 removed-variable list: name → replacement key (normative source)
├── ref-impl-py/
│   └── deployment_config_v1.py        independent canonical writer and digest (standard library only)
├── published/                         generated, checked in, drift-gated, released (below)
│   ├── leyline-daemon-config.schema.json
│   ├── leyline-execution-daemon-config.schema.json
│   ├── leyline-sign-helper-config.schema.json
│   └── leyline-removed-variables.json
└── VECTORS.sha256
```

`ref-impl-py/` is required by `schema-spec/LAYOUT.md` and follows `connections/v1`: its own
reader, canonical writer and BLAKE3, importing no JSON or hashing module. It takes each
vector's *resolved* document (resolution reads the host and is LLO's job; canonicalization and
the digest are what a second runtime must reproduce) and must reach every pinned canonical byte
string and `configDigest`. `verify_deployment_config_digest` runs it, so the vectors are checked
by two implementations, not confirmed by the writer that produced them.

The document's Rust types, parser, resolver, canonical writer and digest live in a
`deployment_config` module of `leyline-schema-spec`, beside `connections`. Turning a resolved
`ExecutionDaemonConfig` into the structs the execution daemon builds is the public library entry
in `leyline-runtime` (D13), and the execution vectors test that entry, so an embedder and the
daemon share one semantics. Every default appears twice and must agree: as the JSON
Schema `default` keyword, and in the resolver. A test (`verify_deployment_config_defaults`)
resolves the empty document of each kind and compares every leaf with the schema's `default`.
Where a default depends on the host (a path under `$HOME`, the mount backend per OS), the
schema states the rule in `x-leyline-default` and the vectors pin its resolution for a fixed
`HOME` on macOS and on Linux.

**Published artifact.** The source files above use `$ref` into `common.schema.json`. The
published form is one self-contained file per kind, `leyline-<kind>.schema.json` (for example
`leyline-execution-daemon-config.schema.json`), generated by inlining the shared `$defs`. It is
JSON written with sorted keys, two-space indentation and the standard JSON escapes (schema
`description` strings contain quotes and non-ASCII, so D6's no-escape rule, which governs
configuration documents, does not apply to schema files); consumers compare these files by
bytes and never re-derive them.

The artifact follows how `execution.tools.json` is produced and released today, which is:
generated by a task (`gen:execution-tools`), checked in, drift-gated by a test that regenerates
and compares (`schema-bridge/tests/execution_v1.rs`, "registry artifact drifted"), staged by
`tools/stage_release_artifacts.sh` from a named source, published by `release.yml`, checked by
`tools/test_release_publication.sh` and listed in `tools/release-assets.txt`. The config schemas
take each step: generated by `task gen:deployment-config-schema` into
`rs/ll-core/schema-spec/deployment-config/v1/published/`; drift-gated by a schema-spec test
that regenerates and compares; embedded in the binaries with `include_str!` (a test asserts
`--print-config-schema` emits exactly the checked-in bytes); staged and published as release
assets, with `test_release_publication.sh` asserting each asset is byte-equal to the checked-in
file. The schemas need no generator binary to consume: unlike `execution.tools.json`, which
cloister's llo-bump regenerates from a `git archive` of `schema-spec` with the published
`capnpc-schema-bridge-tooldefs` plugin (cloister `scripts/llo-bump.mjs:260-291`), these files
are checked in already generated, so llo-bump takes them from the same `git archive` of
`schema-spec` at the release tag and pins each by SHA-256. The release asset is the same bytes,
for consumers without a checkout. The SHA-256 pin is file integrity, not an LLO identity domain.

`leyline-removed-variables.json` is published the same way (generated from
`removed-variables.source.json`, checked in, drift-gated, embedded, released). It is the D3
refusal list as data: `{"apiVersion": "config.leyline/v1", "variables": [{"name":
"LEYLINE_PROFILE", "replacement": "diagnostics.profile"}, ...]}`, sorted by name. The
binaries' refusal and the library's `check_removed_environment` (D13) read the embedded copy,
and a consumer that spawns or embeds LLO derives its scrub or check list from the pinned file,
never from a copy of its own.

### D3 — Precedence: the document or flags, never both

```
effective configuration = (document | flags translated to a document) + schema defaults
```

There is no third source and no merge:

- **`--config` and any configuration flag together are refused** (exit 64, naming the flag).
  An override would make the running configuration differ from the document cloister checked
  and digested, which is the failure this ADR exists to remove. A user who wants to change one
  value writes a second file.
- **Flags remain, as an adapter.** Without `--config`, the daemon translates its flags into a
  document of its kind and processes that document exactly as if it had been read from a file:
  same validation, same defaults, same `--print-config` output, same digest. Flags are
  therefore neither overrides nor deprecated (D10).
- **Action flags are not configuration** and combine with `--config`: `--config`,
  `--check-config`, `--print-config`, `--print-config-schema`, `--expect-config-digest` (D5),
  and `daemon --reset-arena`,
  which is a one-time action on this start ("drop the live db and parse cold",
  `cmd_daemon.rs:131-137`), not a state the deployment holds. Putting it in a persistent
  document would reset the arena on every restart.
- **Environment: no configuration.** After v0.30.0 no daemon, and no one-shot subcommand,
  reads a `LEYLINE_*` or `LLO_*` variable as configuration. Each one either becomes a document
  field or is removed (Migration table). Three host conventions are still read, only to compute
  documented defaults, and `--print-config` shows their result: `HOME`, `XDG_STATE_HOME`, and
  the platform data directory (`dirs::data_dir`). `RUST_LOG` keeps controlling log verbosity,
  which changes what is logged and never what the daemon does. `LEYLINE_STATE_DIR` is removed
  rather than kept: `XDG_STATE_HOME` already relocates the state directory, and the document's
  `stateDir` names it explicitly.
- **Two named test seams stay, and are not configuration.** `LLO_EXTRACTION_EPOCH` and
  `LLO_INJECTIONS_SCM` let one binary act as two releases with different extraction rules; mache's
  tests use the first against the pinned release binary (mache `build/incremental_test.go:86-91`,
  `internal/lloschema/derivation_test.go:31-32`). Both are folded into `Head.parserId` (through
  the extraction epoch and the injection epoch), so what they change is recorded in the head
  they produce, and a reader that disagrees refuses on `parserId`. They stay readable in release
  builds, are not on the refusal list, and every process that sees one set logs a warning naming
  it at start. `LLO_DISABLE_INJECTIONS` is different: it changes extracted facts without entering
  any epoch (its own comment: "NOT a user knob", `injections.rs:150-158`), so a release binary
  with it set writes facts its `parserId` does not describe. It moves behind a non-default
  `test-seams` cargo feature that the release build does not enable, and a release smoke test
  asserts that setting it changes nothing. `LLO_EMBEDDER` is read only in `vec` builds, which
  releases are not, to force the deterministic embedder for tests and offline use; it is a test
  seam of the same kind and moves behind `test-seams` too.
- **A removed variable that is still set is refused**, naming it and its replacement key.
  Without this, a deployment that still exports `LEYLINE_HEAD_SIGNING_KEY` would silently write
  unsigned heads after the upgrade, and one that exports `LEYLINE_SIGN_CALLER_TOKENS` would
  believe its helper authenticated callers. The refusal list is every variable in the Migration
  table marked "removed", published as data (`leyline-removed-variables.json`, D2). It is one
  check at process start, before subcommand dispatch, in both binaries (`leyline` and
  `leyline-sign-helper`), so no entry point can miss it: `rebuild`, `lsp` and `splice` reach
  parse paths too. It runs before the config file is read and before any socket, lock, arena or
  control file is touched, and the process exits at once with status 78. The message is on
  stderr and names **every** offending variable, each with its replacement key, one per line,
  not only the first:

  ```text
  leyline: refusing to start: removed environment variables are set (ADR-0049):
    LEYLINE_PROFILE: use diagnostics.profile in the config document
    LLO_TRUSTED_QUERY_HASHES: use parser.trustedQueryHashes in the config document
  ```

  A process that inherits its environment from a user shell, such as the daemon mache spawns,
  refuses when that shell still exports a removed variable. mache surfaces the refusal rather
  than scrubbing (`mache-0be425`): because the exit is immediate and precedes all socket work,
  mache's start timeout never decides the outcome, and its spawn-failure log already carries
  the child's stderr (`mache-e70109`). The CHANGELOG fragment says so. Embedders that link LLO
  in-process call the same check through the library (D13).

### D4 — Secrets by reference only

A secret field's only accepted value is a reference object. v1 delivers one reference kind:

```json
{"file": "/Users/me/.config/leyline/head-signing.key"}
```

The referenced file must be a regular file, not a symlink, owned by the process's user, with no
group or other permission bits (mode `0600` or `0400`), in a directory that is not writable by
group or other; otherwise `--check-config` and start refuse it (`secret-file-permissions`,
naming the field and path). The check and the read use one descriptor: the file is opened with
no-follow, the open descriptor is `fstat`ed, and its bytes are read from it, so what was checked
is what is read.

**Symlinks: the file is `lstat`ed, its directory is `realpath`ed.** The final component is
never followed (`O_NOFOLLOW`; a symlink there is refused). The parent directory is resolved
with `realpath` first, and the directory checks run on every directory of the resolved path:
each is owned by root or the process's user, and has no group or other write bit unless its
sticky bit is set (as on `/private/tmp`) and the file itself is owned by the process's user, so a path under macOS's `/tmp` or `/var` (symlinks to `/private/tmp` and
`/private/var`) is checked where it really lives, and a parent swapped for a symlink after the
check cannot redirect the read, because the read opens the resolved directory's descriptor and
then the file relative to it (`openat` with `O_NOFOLLOW`). The file is read at start, never copied into the resolved document, and
`--print-config` prints the reference, not the value.

`mcp.tokenFile` keeps today's "generate if absent", with two tightenings: generation creates the
file exclusively with no-follow (a planted symlink at a missing path is refused, never written
through), and an existing empty file is refused rather than overwritten (today it is
regenerated, `auth.rs:58-66`).

**The document file is held to the same rules.** It can turn off the token gate, open a public
bind, select the metadata-only evidence verifier, name trust keys and name the worker binary, so
whoever can write it holds that authority. The `--config` file must therefore pass the same
checks as a secret file, except that it may be readable by others (mode `0644` and `0444` are
accepted; any group or other write bit is not), and it is read once through one descriptor.
Refusal: `config-file-permissions`, exit 78. Today the same authority is in argv, which only
the launcher can set; this rule keeps it there.

**An inlined value is refused by name.** A string, number, array or any object other than
`{"file": ...}` in a secret field is refused with reason `secret-inlined` and the field's
dotted name, before any other check of that field. There is no other place to put a secret:
every non-secret string field has a constrained type (an absolute path, an enum, a label, an IP
address, or 64 lowercase hex characters for a query hash), and no field is free text.

**`secretRef` is a union with reserved variants.** v1 defines `file` and reserves two more
names, `keychain` (the macOS Keychain or a Secret Service item) and `vault` (a secret-store
reference), whose shapes are not yet fixed. A v1 binary refuses a reserved variant by name
(`secret-ref-not-delivered`, naming the field and the variant), not as an unknown key, so the
schema already lists the names and delivering one later is an additive change within v1: the
variant's body gains a schema, a binary that delivers it accepts it, and every document a
v1 binary accepted keeps its meaning and its `configDigest`. Any other key in a `secretRef` is
`unknown-key`. The sign helper already resolves `keychain://`, `op://` and `apple-password://` URLs
for its callers; those are the helper's request surface, not its own configuration.

**Fields that hold a secret today:**

| Secret | Today | v1 field | Notes |
|---|---|---|---|
| Head signing key (Ed25519 seed) | `LEYLINE_HEAD_SIGNING_KEY`, value inline in hex | `head.signingKey` | File holds the 64 hex characters, as the env value does today. Default `null`: heads are written unsigned, as with the variable unset. |
| MCP shared-secret token (ADR-0022) | implicit file `<data_dir>/leyline/daemon.token`, generated `0600` if absent (`auth.rs:46-82`) | `mcp.tokenFile` | Default `{"file": "<data_dir>/leyline/daemon.token"}`. Keeps "generate if absent"; an existing file with group or other bits is now refused rather than read. |
| Sign-helper caller bearer tokens | `LEYLINE_SIGN_CALLER_TOKENS`, value inline | `auth.callerTokens` | File holds today's `name=token,...` grammar (`host/auth.rs`, `AuthConfig::parse`). |

Not secrets, and not secret fields: execution `--trust-key` files and `LEYLINE_HEAD_TRUSTED_KEYS`
(Ed25519 public keys: trust anchors, by path in v1, D5 "Paths"), `--evidence-store` (a CAS
directory), `LLO_TRUSTED_QUERY_HASHES` (public BLAKE3 digests), and
`LEYLINE_SIGN_HEALTHZ_PROBE_URL` (a keystore URL, a reference). Out of scope because LLO never
reads them: `OP_SERVICE_ACCOUNT_TOKEN`, `OP_SESSION_my`, `OP_ACCOUNT`, `OP_DEVICE`, which the
sign helper passes through an allowlist to the `op` subprocess it spawns
(`host/keystore.rs:805-821`), and `GITHUB_TOKEN`, read only by the one-shot `self update`
(`cmd_self.rs:114`).

### D5 — Semantics

**Strictness.**

- The text is JSON (RFC 8259) or a supported adapter's syntax (D7). JSON nesting deeper than
  16 is `malformed`.
- `apiVersion`, then `kind`, are checked first, wherever they appear in the text, with the
  refusals D1 lists (`bad-api-version`, `unknown-kind`, `wrong-kind`).
- An undefined key at any level is refused (`unknown-key`); a key twice in one object is
  refused (`duplicate-key`).
- Every string is UTF-8 and contains none of: U+0000-U+001F, U+007F, U+0080-U+009F, U+2028,
  U+2029, `"` or `\` (`bad-string`). So the canonical writer needs no escape (D6), and Go's
  encoder, which escapes U+2028 and U+2029, never meets one. The rule applies equally to every
  string resolution produces: a `~/` expanded against a `HOME` that is unset, relative, not
  UTF-8 or containing a refused character is refused (`bad-host-default`, exit 78), never
  replaced by `.` as today's arena default is (`main.rs:536-538`, `696-698`).
- Integers are JSON integers with no fraction or exponent, in the field's range
  (`out-of-range`). Every integer field's maximum is at most 2^53 − 1 (9007199254740991), so a
  JavaScript consumer reads every value exactly. No v1 field is a float.
- An IP literal is accepted only in its one spelling: IPv4 as a strict dotted quad (no leading
  zero), IPv6 in RFC 5952 canonical text. Any other spelling of a valid address is refused
  (`non-canonical-address`), as `connections/v1` refuses non-canonical loopback, so one address
  has one resolved byte string.
- `null` is accepted only in a field whose default is `null`, where it means the same as
  omitting the field. `null` in any other field is refused (`null-not-allowed`): it would
  otherwise read as "unset", which v1 does not have.
- A required field (no default) that is absent is `missing-key`. v1 has four at the top of
  `execution-daemon-config`: `cas`, `worker.path`, `catalog` and `evidence`. Three of them
  refuse start today when absent. `evidence` does not: with no verifier flag the daemon starts
  and installs `RejectUnverifiedEvidence`, which refuses every run
  (`cli-lib/src/daemon/execution.rs:122-138`, pinned by
  `without_a_verifier_configuration_every_start_is_refused`, `cli/tests/execution_evidence.rs:374`).
  v1 refuses the document instead, so a daemon that can run nothing is caught at
  `--check-config`; this is a `Breaking` entry and that test is rewritten to pin the new refusal.
  A required key inside an optional object is required when the object is present:
  `mount.path`, `source.path`, `mcp.http.port`, `evidence.verified.trustKeys` and
  `evidence.verified.store`. A union object must have exactly one key: `{}` (which an empty TOML
  table such as `[backend]` produces) is `union-empty`.

**Paths.** Absolute, or beginning `~/`, which resolves against `HOME` (only `~/`; `~user` is
refused). Relative paths are refused (`path-not-absolute`): relative to what is the question
every config format answers differently, and cloister emits absolute paths. No `.` or `..`
component, no empty component, no trailing `/` (the `connections/v1` path rules). Trust keys
(`evidence.verified.trustKeys`, `head.trustedKeys`) are paths to key files, like `--trust-key` today,
so one key format serves both.

**Defaults are explicit.** Every optional field has a default in the schema, and the resolved
document (D6) states it. Cloister writes only the fields it has declarations for (CAS volume,
run root, control socket, placements) and inherits every other default; it learns those
defaults from `--print-config`, not from reading LLO's source.

**Fail-closed defaults stay fail-closed.** Each default that today refuses or narrows is the
schema default, and loosening it takes the same explicit field the flag took:
`evidence` has no default (no verifier, no start); `mcp.http.bind` defaults to `127.0.0.1` and a
non-loopback bind needs `mcp.http.allowPublic: true`; `mcp.noAuth` defaults to `false`;
`backend.microVm.tsiHijackInet` defaults to `false`; `sign-helper` `auth.required` and
`signAllow.required` default to `true` (changed from today's flag defaults; Q5).

**Q4: `mcp.groups` defaults to every group (`null`), as today.** The MCP surface is already
off by default, and each step that exposes it is its own explicit field: a listener
(`mcp.http` or `mcp.uds`, both `null` by default), loopback-only binding unless
`allowPublic`, and the ADR-0022 token gate unless `noAuth`. A default of `[]` would add a fourth
step whose only effect is a listener that exposes nothing, which cloister's e2e smoke (which
enables `--mcp-port` and expects the `lsp_*` tools) and every standalone user would then have
to undo by naming every group; and an empty grant is not a safer state but a broken one, since
the listener still binds and still answers `tools/list`. The opt-in is the listener, not the
group list. Narrowing the default later is allowed within v1 (D9), so this choice does not
lock it in.

**`worker.readyTimeoutMs` defaults to 60000 (60 s), not today's 5000.** 5 s is shorter than
every value LLO and its consumers actually run with: all of LLO's live microVM suites use 30 s
(`krun-worker/tests/*.rs`, `ready_timeout: Duration::from_secs(30)`, and the guest harness
passes `--ready-timeout-ms 30000`, `cli/tests/guest_harness/mod.rs:182`), cloister's in-process
host runtime uses 30 s (cloister `rs/crates/host-runtime/src/main.rs:77` at `80ef046`), and
cloister reports readiness above 30 s under host load, the condition `ley-line-open-c4480a`
documents and `AGENTS.md` ties to load. 60 s is twice the value every working deployment uses,
and still bounds a guest that never becomes ready: the c4480a stall prints nothing in 60 s and
fails closed as a timeout either way. The timeout is a liveness bound, not an authority, so a
longer default weakens no refusal. The flag's default moves with it (the adapter maps an absent
`--ready-timeout-ms` to the document default), a `Changed` entry.

**Broker Slice 1 knobs are constants in v1, not keys** (cloister review F8). The broker that
ships in the same release has two limits: 256 simultaneous connections per run and delivery to
`127.0.0.1` only (ADR-0046 S-Q2 and S-Q3). Neither belongs in this document. The owner fixed
the cap as a constant and placed any future variation in the per-run `connections/v1` document
("a document field may come later as an additive change", S-Q2), which is cloister's per-run
policy, not deployment configuration; and widening the delivered loopback set is a runtime gate
that a passing loopback probe justifies (S-Q3), a code change with evidence, not an operator
choice. So the broker needs no key in v1, and none of its later changes is a breaking change
to this document.

**Loosening is visible.** `--check-config`, `--print-config` and start each write a stderr
notice for every field set to its loosening value: `evidence.unverified`, `mcp.noAuth`,
`mcp.http.allowPublic`, `backend.microVm.tsiHijackInet`, sign-helper `auth.required = false`
and `signAllow.required = false`, matching the warnings start logs today.

**What the document does not carry.** No `RunGrant`, confinement manifest or connections
document, and nothing per run. It does carry standing caller authority, moved unchanged from
flags and environment rather than added: trust keys, `mcp.groups`, `signAllow` and
`resolveAllow`. Cloister's policy chooses those values; LLO enforces them. LLO enforces the
listener, trust-anchor and backend settings the document states; which runs may execute, with
what, is still decided per run by cloister's grant (ADR-0035). The document does not choose a backend for a run: `backend` selects which
backend this daemon supervises, as `--backend` does today, and a grant requiring the other
class is refused as today (`main.rs:587-590`, `645-648`).

**Cross-field rules** the schema cannot express are checked after the per-field rules, in the
order the README lists them:

- `daemon`: `mcp.http.bind` not loopback and `mcp.http.allowPublic` false →
  `public-bind-without-opt-in`. `mcp.groups` naming a group this binary does not have →
  `unknown-mcp-group`.
- `execution-daemon`: the run root must be neither inside nor a parent of (nor equal to) the
  CAS root, under both `cas` branches (`run-root-overlaps-cas-root`). Today only the volume
  branch checks this (`CasVolume::with_run_root`, `cas_volume.rs:840-865`); the directory branch
  accepts `--cas-root /x --run-root /x/runs` (`main.rs:380-391`), which lets a run's writable
  rootfs land inside the immutable CAS root. v1 closes it with a pure path check.
  `backend.native.cgroupRoot` on macOS → `cgroup-root-linux-only`.
- `sign-helper`: `auth.required` true and an effective caller map that is empty →
  `caller-tokens-required`. "Empty" is decided as today, by `AuthConfig::parse` returning
  `Disabled` (`sign/src/host/auth.rs:54-79`), so a `null` reference and a referenced file holding
  `""`, whitespace or `",,"` are refused alike; the file's contents are checked at
  `--check-config` and at start. Likewise `signAllow.required` true and no caller with a
  non-empty prefix list → `sign-allow-required`. `bind` not loopback → `bind-not-loopback`
  (ADR-0019 requirement 2), at parse; today the helper checks it at start
  (`helper.rs:104-111`), after which it binds before running the auth and allow-list refusals,
  and v1 runs every refusal before binding. A `resolveAllow` prefix that would authorize a
  signing-key URL → refused as today (`helper.rs:206-230`).

Unions make the other relations unrepresentable rather than checked: `--device` and
`--tsi-hijack-inet` exist only under `backend.microVm`, `--cgroup-root` only under
`backend.native`, `--trust-key` and `--evidence-store` only under the verified branch of
`evidence`, `--nfs-port` and the mount backend only inside `mount`.

**`--check-config`** reads the document (or the flags), validates it, resolves it, then runs
the read-only host checks, and exits. It writes nothing anywhere: no directory is created
(today's start creates `~/.mache`, `main.rs:536-540`, and the run root, `main.rs:388`,
`cas_volume.rs:908`), no volume attached (`attach_existing` runs `hdiutil attach` and creates
`mnt` and `cas`, `cas_volume.rs:943-1040`), no socket bound, no token generated
(`load_or_generate`, `auth.rs:58-82`), and no case-sensitivity probe run (`probe_sensitivity`
writes a temporary directory and two files even inside an existing run root,
`cas_volume.rs:110-123`). Host checks: the document file and every secret file pass D4; every
referenced file and directory exists with the right type; the catalog parses; trust keys parse
and the evidence store exists, through the same `EvidenceVerifierConfig::build` start uses,
which stays the single verifier gate; the head signing key's file holds a valid seed; the
caller-tokens file and allow-lists parse (`AuthConfig::parse`, `SignAllowList::parse`); the
worker is a regular file; `CgroupRoot::open` accepts the cgroup root; the MCP UDS path is
admissible (`admit_well_known`); the binary was built with what the document needs (`mount`
needs the `mount` feature, the `fuse` backend the `fuse` feature); the NFS mount's macOS
version gate (`storefs/src/host/mod.rs:112-132`); the platform supports a CAS volume. Checks
that need a write, the run root's case sensitivity and attaching the volume, are listed on
stderr as deferred to start, and do not fail the check. So exit 0 means "this document is valid
and every read-only check passed", not that start cannot fail.

| Exit | Meaning |
|---|---|
| 0 | Valid, and this host can run it. stdout: one line, `configDigest blake3-256:<hex>`. |
| 64 | Usage: `--config` with a configuration flag, or a flag the adapter refuses (named). |
| 65 | The document is refused (`EX_DATAERR`). |
| 66 | The file cannot be read (`EX_NOINPUT`). |
| 78 | The document is valid but this host or binary cannot run it (`EX_CONFIG`). |

Refusals go to stderr, one JSON object per line:
`{"field":"mcp.http.bind","reason":"public-bind-without-opt-in","message":"..."}`. A document
refusal reports the first refusal in the fixed order above, so two runs report the same one;
host refusals report every failed check, sorted by field. `field` is dotted from the document
root, empty for a whole-document refusal. Reason codes are stable within a major version.

**`--print-config`** does everything `--check-config` does and, on success, writes the resolved
document's canonical bytes (D6) to stdout instead of the digest line. On failure it writes
nothing to stdout. The digest of exactly those stdout bytes is the `configDigest`.

**At start**, the daemon processes the document as `--check-config` does, then logs the
`configDigest` once, before it binds anything. The action flag `--expect-config-digest
blake3-256:<hex>` makes start refuse (exit 78, `config-digest-mismatch`) when the resolved
document's digest differs, so a launcher that checked a document can require that the same
document runs, closing the window between its `--check-config` and the start.

### D6 — Canonical form and `configDigest`

The resolved document is the document with every default filled in, `~/` expanded, and lists
in canonical order. Its canonical bytes follow `confinement/v1` §7 rules 1-4, as
`connections/v1` does: UTF-8 with no BOM; keys sorted in byte order at every level; two-space
indentation; no trailing whitespace or newline. Four differences from `connections/v1`, all
stated in the README:

1. **`null` is written, not omitted.** In the resolved document every key is present, because
   an omitted key in the input means "LLO's default", and the resolved document exists to show
   what that default is. `"mount": null` is how the resolved document says "no mount".
2. **Strings are UTF-8, not printable ASCII.** `connections/v1` rule 4 allows printable ASCII
   only. A deployment's paths contain the user's home directory, which is not ASCII for many
   users, so v1 allows UTF-8 minus the characters D5 refuses. Strings are written raw, with no
   escape: Python's `json.dumps` needs `ensure_ascii=False`, Go's encoder needs
   `SetEscapeHTML(false)` (and never meets U+2028 or U+2029, which D5 refuses).
3. **Byte order means UTF-8 byte order**, for keys and for sorted lists, which equals Unicode
   code point order. It is not UTF-16 code unit order: JavaScript's default `Array.prototype.sort`
   puts U+1F600 before U+FF61, and a Node consumer must compare code points.
   `test-vectors/canonical-strings.json` pins a list that tells the two orders apart.
4. **Lists that are sets are sorted** (`trustKeys`, `devices`, `runtimeFiles`, `mcp.groups`,
   `parser.trustedQueryHashes`, `resolveAllow`, each `signAllow` caller's prefixes); a duplicate
   entry is refused (`duplicate-entry`), never merged. A one-element list stays a list. An empty
   list is written `[]` and an empty object `{}`, with nothing between the brackets, as Python's
   `json.dumps(indent=2)` writes them.

Integers are written in decimal with no sign, fraction, exponent or leading zero.

`configDigest` is BLAKE3-256 over those bytes (σ_bytes), written `blake3-256:<hex>`. It names
one resolved configuration of one LLO release on one host: a home directory, an OS default or a
new field in a later release changes it, as it should. **It is not an authority.** It is never
signed by LLO on its own, never carried in a grant or a head, and nothing is admitted or refused
by comparing it, except by `--expect-config-digest`, which compares a launcher's own earlier
result and grants nothing. It is *recorded* as evidence (below): a receipt that names it says
which configuration the run executed under, and grants nothing by doing so. Its text form is the one CAS addresses use, but it is never resolved
against or admitted into any CAS. It covers the document only, not the bytes of the files the
document names (catalog, trust keys, worker binary, evidence store): two starts with one
`configDigest` and a replaced catalog run differently. It is a fingerprint: cloister compares
the digest it got from `--print-config` with the one the daemon logs (or passes it to
`--expect-config-digest`), to know the running document is the checked one. A
cloister-emitted document and a hand-written one that resolve to the same configuration have
the same `configDigest`, whatever each spelled out or omitted, and whichever syntax (D7) each
used. On implementation it gets a row in the `AGENTS.md` vocabulary table, which also states
that it is never resolved against a CAS.

**Where `configDigest` appears** (owner, Q6; cloister review F7). Three places, each additive:

| Surface | Field | Contract change |
|---|---|---|
| Daemon `status` reply | `StatusResponse.configDigest @13 :Text $Json.name("config_digest")`, `blake3-256:<hex>` | `rs/ll-core/public-schema/capnp/daemon.capnp`, next free ordinal after `rootPublished @12`; regenerated Go and TypeScript clients; `compatibility.json`; consumer fixture. Both daemon kinds set it. |
| `RunReceipt` | `configDigest @17 :DigestRef $Traits.optional`, `algorithm` `blake3-256` | `execution/v1/execution.capnp`. `@15` and `@16` were taken by ADR-0046 Slice 1 (`connectionsDigest`, `connections`, PR #576), so `@17` is the next free ordinal on `origin/integration` at `e6142dd4`, and no open branch claims it. Regenerated `execution.tools.json`, Go and TypeScript bindings, `compatibility.json` and the receipt vectors. |
| In-process `RunOutcome` | `config_digest: Option<DigestRef>` (`runtime/src/model.rs:671`) | A Rust API addition for embedders such as cloister's host runtime, which read the outcome rather than a receipt. |

The receipt and the outcome carry it when the service or backend was built from a resolved
document, through the daemon or the library entry (D13); a daemon always is (flags become a
document), so every daemon receipt names its configuration. An embedder that builds a backend
by hand, without a document, gets none, and the field is absent rather than invented. Because
the receipt is the digest-bound evidence a run produces (ADR-0046 S-Q4), an attester that
covers the receipt's digest covers the configuration's name with it; the configuration's own
named files (catalog, trust keys, worker) are covered only to the extent the receipt already
names them (the rootfs digest in `inputRoots`, the evidence it verified).

### D7 — Adapters: TOML in v0.30.0; YAML and HCL later

An adapter is a function from its syntax to a JSON value, followed by the same parse, resolve
and canonicalization as native JSON. There is one data model, so a TOML document and its JSON
twin have the same resolved bytes and the same `configDigest`. The adapter is chosen by file
extension: `.json` and `.toml`. Any other extension is refused (`unknown-config-syntax`), so a
file's syntax is never guessed from its content.

**TOML → JSON** (`toml` 1.x, already in `rs/Cargo.lock`): tables become objects, arrays become
arrays, strings and booleans map directly, integers map when in range. A TOML datetime or float
is refused (`toml-type-not-mapped`) because no v1 field has either type. TOML cannot express
`null`; an omitted key is the default, which is what `null` would have meant. TOML's own
duplicate-key rule refuses repeats before the mapping runs.

**YAML and HCL are not in v0.30.0.** YAML's implicit typing (`no` as false, `010` as octal,
unquoted `1.0` as a float), anchors and merge keys are each a way for two spellings of one
document to resolve differently; an adapter has to refuse them, which is its own design and
vector set. HCL needs a block-to-object convention. Both are additive later: a new adapter adds
an extension and a vector file, and changes no schema, field or digest. Cloister needs neither.

### D8 — The standalone example

`docs/examples/deployment-config/daemon-mount-macos.toml`, hand-written and checked in:

```toml
apiVersion = "config.leyline/v1"
kind = "DaemonConfig"

[source]
path = "~/src/my-project"

[mount]
path = "~/mnt/my-project"
# backend defaults to "nfs" on macOS; nfsPort defaults to 0 (the kernel picks a port).
```

Its JSON twin `daemon-mount-macos.json` sits beside it, and `daemon-mount-macos.resolved.json`
pins the resolved document for `HOME=/Users/fixture` on macOS. Run it with
`leyline daemon --config daemon-mount-macos.toml`.

Tests (`rs/ll-open/cli/tests/deployment_config.rs`):

- `--print-config` of the TOML and of the JSON, with `HOME=/Users/fixture`, both equal the
  pinned resolved file byte for byte (runs on every platform: resolution is pure; the test
  sets the platform the resolver assumes).
- `--check-config` of the example on a source build without the `mount` feature exits 78 with
  `mount-feature-not-built`; on a release-configured build it exits 0.
- On macOS with a `mount` build, a gated test starts `leyline daemon --config` from the example
  pointed at a temporary source and mount directory, reads one file through the mount, and
  stops the daemon. It runs in the existing mount smoke tier (`cli-lib:smoke:dev`) under its
  platform gate, which refuses macOS releases whose NFS client is unsafe.

The example runs from a release download because v0.30.0 release binaries carry `mount` (owner,
Q3): `release.yml` builds `leyline` with `--features mount` on every target (the feature is pure
Rust and links no system library, `cli/Cargo.toml:18-20`), `fuse` stays opt-in, and the release
gate checks that a staged binary's `leyline capabilities` reports the mount backend
(implementation slice). The `--check-config` refusal `mount-feature-not-built` remains for
source builds without the feature.

### D9 — Versioning and breaking changes

- **Within v1:** adding an optional field with a default is not breaking for a document. A
  newer binary accepts every older v1 document; an older binary refuses a document using the new
  field (`unknown-key`), which fails closed. The resolved bytes, and so `configDigest`, gain the
  new key; cloister sees this at its llo-bump by comparing `--print-config` output.
- **Narrowing a default** within v1 (a tighter bound, a deny where there was an allow) is a
  `Breaking` CHANGELOG entry and changes the pinned resolved vectors in the same pull request,
  because a document that omitted the field changes behaviour. It does not need v2: the
  document still means "LLO's default", the change fails closed, and the consumer's pinned
  `--print-config` test catches it.
- **Loosening a default** is v2, never a v1 change. A document that omitted `mcp.noAuth`,
  `mcp.http.allowPublic`, `backend.microVm.tsiHijackInet`, sign-helper `auth.required` or
  `signAllow.required`, or any other field whose default refuses or narrows, would otherwise
  be weakened under an unchanged `apiVersion`. The schema marks such fields
  `"x-leyline-fail-closed": true`, and a schema-spec test compares each one's default with the
  previous release's published schema and fails when it loosened without a major version.
- **Removing, renaming or retyping a field, or changing a field's meaning,** is v2 for that
  kind (`apiVersion` `config.leyline/v2`, same `kind`). A binary that has moved a kind to v2
  refuses its v1 documents with `bad-api-version` naming both; there is no reading of two
  versions, and no conversion.
- **Delivering a reserved `secretRef` variant** (`keychain`, `vault`, D4) is additive within
  v1: the names are already in the schema and refused by name until delivered.
- **Removed environment variables** are removed in v0.30.0, not deprecated, each listed under
  `Removed` in the CHANGELOG fragment with its replacement key, and refused when still set (D3).

### D10 — Flags: kept as the flags adapter, not overrides, not deprecated

Decided (owner, Q2): every configuration flag stays in v0.30.0 as the adapter D3 describes, and
`--config` with a configuration flag is refused. Reasons:

- mache spawns `leyline daemon --arena … --arena-size-mib 64 --control … [--source …]
  [--cdc --cdc-target source-blobs] [--reset-arena]` (mache `internal/leyline/socket.go:407-460`,
  read at its current main); removing flags would break every installed mache at the pin bump,
  for no gain, since the adapter yields the same document.
- LLO's own tests start both daemons with flags; they keep doing so and gain document coverage
  through the adapter.
- "Remove, don't deprecate" applies to what this ADR removes (the environment variables and the
  vestigial `--foreground`, below), cleanly and in one release. The flags are not deprecated:
  they are a supported second spelling of the same document. Removing them later is a separate
  owner decision, taken once no consumer spawns with them.

Adapter rules that change flag behaviour, each a `Changed` CHANGELOG entry:

- A flag that configures nothing is refused rather than ignored: `--backend` or `--nfs-port`
  without `--mount`; `--mcp-bind` or `--mcp-allow-public` without `--mcp-port`; on the execution
  daemon `--tsi-hijack-inet` and the three `--workspace-*-ms` flags with `--backend native`
  (the native branch never reads them, `main.rs:559-606`). These are ignored today
  (`cmd_daemon.rs:93-99`, `356-358`). The adapter tells a flag given from a clap default with
  `ArgMatches::value_source`, so a defaulted `--workspace-ttl-ms` is not "given".
  `--cdc-target` without `--cdc` stays accepted and inert (`main.rs:807-817`): it is
  single-valued and mache passes it only with `--cdc`.
- Every refusal between flags that exists today stays a refusal (exit 64) and is never
  resolved by choosing one branch of a union: `--allow-unverified-evidence` with `--trust-key`
  or `--evidence-store` (`main.rs:179`; choosing `unverified` would silently drop the trust
  keys and downgrade the verifier), `--trust-key` without `--evidence-store` and the reverse
  (`main.rs:186`, `193`), `--device` with `--backend native` (`main.rs:561`), `--cgroup-root`
  with `--backend micro-vm` (`main.rs:608`), `--cas-volume-size-cap-gib` with `--cas-root`
  (`main.rs:377`), and `--cas-root` with `--cas-volume` (`main.rs:83-87`). The existing
  `a_partial_or_invalid_verifier_configuration_refuses_startup`
  (`cli/tests/execution_evidence.rs:391-434`) keeps passing through the adapter.
- `--timeout 0s` keeps meaning "shut down at once" (`shutdownAfterSeconds` = `0`; daemon tests
  use it, `cmd_daemon.rs:3891`).
- The sign helper's exit codes change from today's 1, 2 and 3 (`helper.rs:93-140`) to D5's 64,
  65, 66 and 78; supervisor units that match on them must be updated (the cloister bead in D12).
- `leyline-sign-helper` keeps no flags except the action flags: its one deployer, cloister,
  migrates in the same pin bump (D12), so it moves to `--config` only. Its `--foreground` flag
  is never read (`helper.rs:65-68` declares it `default_value_t = true` on a `bool`, so it can
  never be false) and is removed.

### D11 — The cloister consumer contract

Cloister (`cloister-55e6cd`) is the primary producer. Its checklist, and how this ADR meets
each item:

| # | Cloister needs | Met by |
|---|---|---|
| 1 | Every default explicit in the schema | D2: every optional field carries a JSON Schema `default` (or `x-leyline-default` for a host rule), and `verify_deployment_config_defaults` fails if the resolver and the schema disagree. |
| 2 | An omitted field means LLO's default | D5 "Defaults are explicit": v1 has no "unset"; `null` only where the default is `null`, refused elsewhere. |
| 3 | `--check-config` / `--print-config` print the fully resolved document | D5: `--print-config` writes the resolved canonical document (every key, defaults and `~/` expanded); `--check-config` validates the same and prints its `configDigest`. |
| 4 | Secrets by reference only | D4: `{"file": …}` only; an inlined value is refused as `secret-inlined`, naming the field. |
| 5 | The migration table covers every flag and variable cloister passes today | Below, and the "Migration table". |
| 6 | The schema is a digest-pinnable artifact like `execution.tools.json` | D2 "Published artifact": generated, checked in, drift-gated, embedded, released, pinned by SHA-256 at llo-bump. |
| 7 | An embedder that links `leyline-runtime` in-process gets the same semantics as the daemon (F1) | D13: a public library entry resolves an `ExecutionDaemonConfig` into the structs the daemon builds; the vectors test it; the daemon uses it. |
| 8 | The removed-variable check and list are usable without the binary (F2) | D13 `check_removed_environment`; D2 `leyline-removed-variables.json`. |
| 9 | Symlinked directories such as macOS `/tmp` are handled (F3) | D4: `lstat` the file, `realpath` the directory, check every resolved directory. |
| 10 | A ready timeout that survives load (F4) | D5: `worker.readyTimeoutMs` defaults to 60000. |
| 11 | `secretRef` extensible without a break (F5) | D4: `keychain` and `vault` reserved, refused by name in v1. |
| 12 | Start pinned to the checked document (F6) | D5 `--expect-config-digest`. |
| 13 | Each run's evidence names its configuration (F7) | D6: `RunReceipt.configDigest @17`, `RunOutcome.config_digest`, `StatusResponse.configDigest @13`. |
| 14 | No breaking config change when the broker ships (F8) | D5: the Slice 1 cap and delivered loopback set are constants, not keys. |

**What cloister uses today** (read at cloister `main` = `80ef046`, read-only, through the GitHub
API; an earlier draft read a stale side branch). Cloister does not launch `leyline
execution-daemon`. Its host runtime, `cloister-host-runtime`, links `leyline_runtime`
in-process and builds the backend itself:

| Cloister site (`rs/crates/host-runtime/src/`) | What it sets today | `ExecutionDaemonConfig` key |
|---|---|---|
| `main.rs:54-84` `krun_config()` | backend is `KrunWorkerBackend` (microVM) | `backend.microVm` |
| `main.rs:61-64` | `worker` from `CLOISTER_KRUN_WORKER`, default `/usr/local/libexec/leyline-krun-worker` | `worker.path` |
| `main.rs:65` | `cas_root` from `CLOISTER_CAS_ROOT`, default `/var/lib/cloister/cas` | `cas.directory.root`, or derived from `cas.volume` (below) |
| `main.rs:66` | `ephemeral_root` from `CLOISTER_EPHEMERAL_ROOT`, default `/var/lib/cloister/run` | `cas.directory.runRoot` / `cas.volume.runRoot` |
| `main.rs:72-75` | `runtime_files` = `[CLOISTER_LIBKRUNFW]` if set | `worker.runtimeFiles` |
| `main.rs:76` | `devices` = `[]` | `backend.microVm.devices` (default `[]`) |
| `main.rs:77` | `ready_timeout` = 30 s | `worker.readyTimeoutMs` (default now 60000, D5) |
| `main.rs:82` | `tsi_hijack_inet` = `false`, deliberately not an env var | `backend.microVm.tsiHijackInet` (default `false`) |
| `main.rs:358-361`, `446-450` | `CasVolume::new(<state dir>, DEFAULT_SIZE_CAP_BYTES)`, state dir from `cas-attach --state-dir` or `CLOISTER_CAS_STATE_DIR`; `attach_existing` checks the attached root equals `cas_root` | `stateDir` + `cas.volume` (`sizeCapGib` default 64); the CAS root is the volume's, so the equality check becomes unrepresentable rather than checked |
| `main.rs:222`, `297-314` | `Platform::host()` for `import_oci_layout` / `pull_oci_image` | none: image import is a one-shot operation, not deployment configuration |
| `execution.rs:17-24` | `KrunWorkerBackend`, `RunOutcome`, `confinement_manifest` | `RunOutcome.config_digest` (D6) when built through D13 |

Cloister's verifier, catalog and authorization live in cloister on this path; the
`evidence`, `catalog` and `control` keys matter to cloister only if it moves onto the daemon,
which is its owner's decision (F1) and is not blocked by D13. The `CLOISTER_*` variables are
cloister's own configuration; LLO reads none of them.

Cloister's launches of LLO binaries on `80ef046` are unchanged from the earlier read:

| Cloister site | LLO process and argv | v1 key |
|---|---|---|
| `scripts/e2e-smoke.sh:76-81` | `leyline daemon --arena <work>/test.arena` | `arena.path` |
| same | `--control <work>/test.ctrl` | `control` |
| same | `--mcp-port <port>` | `mcp.http.port` |
| same | `--mcp-no-auth` | `mcp.noAuth` |
| same | `--timeout 120s` | `shutdownAfterSeconds` = `120` |
| `Taskfile.yml:617` (`helper:start`) | `leyline-sign-helper --bind 127.0.0.1:8786` | `bind` |
| same | `--foreground` | removed (D10); `helper:start` must drop it |
| `GETTING-STARTED.md:677-772` | `--require-auth`, `--require-sign-allow`, `LEYLINE_SIGN_CALLER_TOKENS`, `LEYLINE_SIGN_SIGN_ALLOW`, `LEYLINE_SIGN_OP_BIN`, `LEYLINE_SIGN_SECURITY_BIN` (documented for operators) | `auth.required`, `signAllow.required`, `auth.callerTokens`, `signAllow.callers`, `subprocess.opBin`, `subprocess.securityBin` |

No cloister file on `80ef046` passes `--cas-root`, `--cas-volume`, `--run-root`, `--worker`,
`--catalog`, `--trust-key` or `--evidence-store`, or sets `LEYLINE_STATE_DIR`.

### D12 — Consumer beads

| Repo | Bead | What it needs |
|---|---|---|
| cloister | `cloister-55e6cd` (exists) | Emit `ExecutionDaemonConfig` (and `DaemonConfig` for the e2e smoke) from `cluster.toml`, pin the schemas and `leyline-removed-variables.json` at llo-bump, and either resolve the document in-process through D13 (building `KrunWorkerBackend` from its result instead of `krun_config()`) or move onto the daemon with `--config`; test against `--print-config`. |
| cloister | `cloister-0bc3d5` (filed with this ADR) | `task helper:start`, the supervisor templates and `GETTING-STARTED.md` (lines 677-772 at `80ef046` set `LEYLINE_SIGN_*` and pass `--require-auth`; `Taskfile.yml:617` passes the removed `--foreground`) move to `--config`; the caller-tokens file replaces the env value; dev mode states `auth.required = false` explicitly. Must land at the same pin bump as LLO's sign-helper slice, or the helper refuses to start (D3, removed variables). |
| mache | `mache-0be425` (filed with this ADR) | mache's spawn argv (`internal/leyline/socket.go:405-460`) uses only flags v0.30.0 keeps, and mache's own code sets no variable LLO reads as configuration (`LEYLINE_SOCKET` is mache's; LLO reads it only in `cli/tests/daemon_startup.rs:71`). Its tests' `LLO_EXTRACTION_EPOCH` keeps working (D3, test seams). What changes: the daemon mache spawns inherits the user's shell, so a still-exported removed variable makes it refuse to start. mache surfaces the refusal (its choice, recorded on the bead) rather than scrubbing; D3 gives it an immediate exit before any socket work, with every offending variable and its replacement named on stderr. Moving the spawn to `--config` is optional. |

### D13 — The library entry: one semantics for the daemon and embedders

Cloister's host runtime links `leyline-runtime` and builds `KrunWorkerConfig` itself (D11).
If only the daemon understood the document, an embedder would re-derive the defaults and
mapping by hand and drift. So the execution document has a public library entry in
`leyline-runtime`, and the daemon uses the same entry:

```rust
// leyline_runtime::deployment
pub struct HostFacts { home, xdg_state_home, data_dir, os }   // injected, so resolution is pure
impl HostFacts { pub fn current() -> Result<Self, ConfigRefusal> }

pub fn resolve_execution_deployment(
    text: &[u8], syntax: Syntax /* Json | Toml */, host: &HostFacts,
) -> Result<ExecutionDeployment, ConfigRefusal>;

pub struct ExecutionDeployment {
    pub backend: BackendDeployment,          // Native { config: NativeWorkerConfig, cgroup_root: Option<PathBuf> }
                                             // | MicroVm { config: KrunWorkerConfig, workspace: Freshness }
    pub cas: CasDeployment,                  // Directory { root } | Volume { state_dir, size_cap_bytes, run_root }
    pub catalog: PathBuf,
    pub evidence: EvidenceDeployment,        // Unverified | Verified { trust_keys, store }
    pub arena: PathBuf, pub control: PathBuf,
    pub source: Option<SourceDeployment>, pub cdc: bool,
    pub parser: ParserSettings, pub head: HeadSettings, pub diagnostics: Diagnostics,
    pub resolved: Vec<u8>,                   // the canonical resolved bytes (D6)
    pub config_digest: DigestRef,            // blake3-256 over `resolved`
}

pub fn check_removed_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<(), RemovedVariables>;           // every offender with its replacement key
pub const REMOVED_VARIABLES_JSON: &str;     // the embedded leyline-removed-variables.json
```

- **What it returns is exactly what the daemon builds.** `KrunWorkerConfig` and
  `NativeWorkerConfig` are filled with the same `worker`, `cas_root`, `ephemeral_root`,
  `runtime_files`, `devices`, `ready_timeout` and `tsi_hijack_inet` the daemon passes today
  (`main.rs:564-627`); for `cas.volume`, `cas_root` is the volume layout's CAS root, computed
  without attaching. The function is pure, with no filesystem access; host checks (D5) are a
  separate `check_execution_host(&ExecutionDeployment)` that is read-only
  and that `--check-config` and embedders both call. Writing steps (attach, create the run root)
  stay where they are today.
- **The vectors test this function.** `execution-daemon-config-cases.json` pins, per accept
  case, the resolved bytes, the `configDigest`, and the backend struct values; a
  `leyline-runtime` test runs every case through `resolve_execution_deployment`, so the daemon
  path and the embedder path are tested by one test. The daemon's `--config` calls it, then
  builds the verifier from `evidence` with `EvidenceVerifierConfig::build` as today.
- **Removed variables.** The library never reads the process environment on its own:
  `HostFacts::current()` reads only `HOME`, `XDG_STATE_HOME` and the platform data directory.
  An embedder calls `check_removed_environment(std::env::vars_os())` at its own start, and
  gets the same refusal, naming every offender, that the binaries print. A backend built from
  an `ExecutionDeployment` sets `RunOutcome.config_digest` (D6).
- **Layering.** The data model, parser and canonical writer stay in `leyline-schema-spec`
  (`leyline-runtime` already depends on it, `runtime/Cargo.toml:29`); `leyline-runtime` adds
  only the mapping into its own structs. `DaemonConfig` and `SignHelperConfig` have no embedder
  and get no runtime entry.

## Migration table

Every configuration flag of the three processes, and every environment variable a shipped
binary reads at runtime. Keys are dotted from the document root. "Host default" means the
schema states a rule (D2 `x-leyline-default`) and `--print-config` shows its value.

### `leyline daemon` → `DaemonConfig` (18 flags)

| Flag | Key | Type | Default | Notes |
|---|---|---|---|---|
| `--arena` | `arena.path` | path | host default `~/.mache/default.arena` | `main.rs:696-700`. |
| `--arena-size-mib` | `arena.sizeMib` | integer ≥ 1 | `64` | Pinned by `default_arena_size_is_64_mib_known_scale_limit`. |
| `--control` | `control` | path | host default: `arena.path` with extension `.ctrl` | `lifecycle.rs:56-59`. |
| `--mount` | `mount.path` | path | `mount` is `null` (headless) | The `mount` object exists only when mounting. |
| `--backend` | `mount.backend` | `"nfs"` \| `"fuse"` | host default: `nfs` on macOS, `fuse` on Linux | `lifecycle.rs:17-23`. Without `--mount`: refused (D10). |
| `--nfs-port` | `mount.nfsPort` | integer 0-65535 | `0` (kernel picks) | Without `--mount`: refused (D10). |
| `--language` | `source.language` | label | `null` (detect per file) | Checked against the binary's compiled languages (host check). |
| `--timeout` | `shutdownAfterSeconds` | integer ≥ 0 | `null` (no automatic shutdown) | The flag's `30s`/`5m`/`2h` strings become seconds (`lifecycle.rs:25-29`); `0` shuts down at once, as `0s` does today. |
| `--source` | `source.path` | path | `source` is `null` | |
| `--mcp-port` | `mcp.http.port` | integer 1-65535 | `mcp.http` is `null` (no HTTP listener) | |
| `--mcp-bind` | `mcp.http.bind` | IP literal | `127.0.0.1` | Without `--mcp-port`: refused (D10). |
| `--mcp-allow-public` | `mcp.http.allowPublic` | boolean | `false` | Required for a non-loopback bind (D5). |
| `--mcp-no-auth` | `mcp.noAuth` | boolean | `false` | ADR-0022. Covers both listeners, as today. |
| `--mcp-uds` | `mcp.uds` | path | `null` (no UDS listener) | |
| `--mcp-groups` | `mcp.groups` | list of labels, sorted | `null` (every group) | Comma list becomes an array. Default reasoned in D5 ("Q4"). |
| `--reset-arena` | (none) | | | **CLI-only**: an action on one start (D3). Combines with `--config`. |
| `--cdc` | `cdc.enabled` | boolean | `false` | |
| `--cdc-target` | `cdc.target` | `"source-blobs"` | `"source-blobs"` | Inert when `cdc.enabled` is false (`main.rs:807-817`). |

### `leyline execution-daemon` → `ExecutionDaemonConfig` (23 flags)

| Flag | Key | Type | Default | Notes |
|---|---|---|---|---|
| `--arena` | `arena.path` | path | host default `~/.mache/execution.arena` | `main.rs:542`. Arena size stays fixed at 64 MiB (no flag today, no key). |
| `--control` | `control` | path | host default: `arena.path` with `.ctrl` | |
| `--cas-root` | `cas.directory.root` | path | **required** (`cas` is one of `directory` or `volume`) | `main.rs:83-88`. |
| `--run-root` | `cas.directory.runRoot` / `cas.volume.runRoot` | path | required under `directory`; under `volume` host default: the volume's own (`<state>/cas-volume/mnt/runs` on macOS, `<state>/runs` on Linux) | `main.rs:109-116`. |
| `--cas-volume [STATE_DIR]` | `cas.volume` (object) | object | **required** (or `directory`) | The flag's optional value becomes `stateDir` below. |
| (`--cas-volume`'s value) | `stateDir` | path | host default: `$XDG_STATE_HOME/leyline`, else `~/.local/state/leyline` | Top level, so `rootfs provision-cas --config` reads the same value (D1). Replaces `LEYLINE_STATE_DIR`. |
| `--cas-volume-size-cap-gib` | `cas.volume.sizeCapGib` | integer ≥ 1 | `64` | `DEFAULT_SIZE_CAP_BYTES`. Unrepresentable under `directory` (today a hand check, `main.rs:377`). |
| `--worker` | `worker.path` | path | **required** | |
| `--runtime-file` | `worker.runtimeFiles` | list of paths, sorted | `[]` | |
| `--ready-timeout-ms` | `worker.readyTimeoutMs` | integer ≥ 1 | `60000` (today `5000`) | D5: every live suite and cloister run with 30 s; readiness exceeds 30 s under load. |
| `--catalog` | `catalog` | path | **required** | The trusted catalog stays its own document; this names it. |
| `--backend` | `backend` (union key) | `{"native": {…}}` \| `{"microVm": {…}}` | `{"native": {}}` | |
| `--device` | `backend.microVm.devices` | list of paths, sorted | `[]` | Unrepresentable under `native` (today `main.rs:561`). |
| `--tsi-hijack-inet` | `backend.microVm.tsiHijackInet` | boolean | `false` | Weakens isolation; explicit only. |
| `--cgroup-root` | `backend.native.cgroupRoot` | path | `null` (wall clock only) | Linux only (D5). |
| `--cdc` | `cdc.enabled` | boolean | `false` | |
| `--allow-unverified-evidence` | `evidence.unverified` | `{}` | **required** (`evidence` is one of `unverified` or `verified`, no default) | With `--trust-key` or `--evidence-store`: exit 64 (D10). |
| `--trust-key` | `evidence.verified.trustKeys` | list of paths, ≥ 1, sorted | (required within `verified`) | Public keys; not secrets (D4). |
| `--evidence-store` | `evidence.verified.store` | path | (required within `verified`) | |
| `--source` | `source.path` | path | `source` is `null` | |
| `--language` | `source.language` | label | `null` | |
| `--workspace-ttl-ms` | `backend.microVm.workspace.ttlMs` | integer ≥ 0 | `60000` | ADR-0044 read freshness; the share exists only on `microVm` with `source`. |
| `--workspace-changed-ttl-ms` | `backend.microVm.workspace.changedTtlMs` | integer ≥ 0 | `1000` | |
| `--workspace-change-window-ms` | `backend.microVm.workspace.changeWindowMs` | integer ≥ 0 | `60000` | |

### `leyline-sign-helper` → `SignHelperConfig` (5 flags)

| Flag | Key | Type | Default | Notes |
|---|---|---|---|---|
| `--bind` | `bind` | `IP:port`, loopback | `127.0.0.1:8786` | Loopback checked at parse, before any bind (D5); today at start (`helper.rs:104-111`). |
| `--rate-limit` | `rateLimitPerSecond` | integer ≥ 1 | `1000` | Per-UID, ADR-0019 req. 10. |
| `--foreground` | (none) | | | **Removed**: never read, cannot be false (D10). |
| `--require-auth` | `auth.required` | boolean | `true` (today `false`) | Default-deny; dev mode states `false`. Flag removed (D10). |
| `--require-sign-allow` | `signAllow.required` | boolean | `true` (today `false`) | Same. Flag removed. |

### Environment variables read at runtime (24)

| Variable | Read at | Key / disposition | Type | Default |
|---|---|---|---|---|
| `LEYLINE_STATE_DIR` | `runtime/src/cas_volume.rs:139-157` | **removed** → `stateDir` | path | host default (above) |
| `LEYLINE_HEAD_SIGNING_KEY` | `cli-lib/src/cmd_parse.rs:3016` | **removed** → `head.signingKey` | secretRef (D4) | `null` |
| `LEYLINE_HEAD_TRUSTED_KEYS` | `cmd_parse.rs:2994` | **removed** → `head.trustedKeys` | list of key-file paths, sorted | `[]` |
| `LEYLINE_HEAD_REQUIRE_SIGNATURE` | `cli-lib/src/head_claim.rs:195` | **removed** → `head.requireSignature` | boolean | `false` |
| `LEYLINE_PROFILE` | `cmd_parse.rs:2326`, `daemon/ops.rs:953` | **removed** → `diagnostics.profile` | boolean | `false` |
| `LEYLINE_SESSION_CORPUS` | `daemon/session_observation_pass.rs:336` | **removed** → `enrichment.sessionCorpus` (daemon kinds) | path | `null` (pass does nothing) |
| `LEYLINE_SHEAF_ABLATION_LOG` | `daemon/sheaf_ablation.rs:79` (name at `:49`) | **removed** → `diagnostics.sheafAblationLog` | path | `null` |
| `LEYLINE_INSTALL_DIR` | `cmd_self.rs:107` | **CLI-only**: becomes `self update --install-dir` | path | as today |
| `LEYLINE_SIGN_CALLER_TOKENS` | `sign/src/bin/helper.rs:143` | **removed** → `auth.callerTokens` | secretRef (D4) | `null` |
| `LEYLINE_SIGN_RESOLVE_ALLOW` | `helper.rs:191` | **removed** → `resolveAllow` | list of URL prefixes, sorted | `[]` (deny all, as today) |
| `LEYLINE_SIGN_SIGN_ALLOW` | `helper.rs:234` | **removed** → `signAllow.callers` | object: caller label → sorted list of URL prefixes | `{}` |
| `LEYLINE_SIGN_RESOLVE_CACHE_MAX` | `host/keystore.rs:240` | **removed** → `resolveCache.maxEntries` | integer ≥ 1 | `1024` |
| `LEYLINE_SIGN_RESOLVE_TTL_MS` | `keystore.rs:435` | **removed** → `resolveCache.ttlMs` | integer ≥ 0, or `null` | `null` (per scheme: 60000 for `op://` and `apple-password://`, 0 otherwise, `keystore.rs:425-445`) |
| `LEYLINE_SIGN_OP_BIN` | `keystore.rs:600` | **removed** → `subprocess.opBin` | path | `null` (`op://` unavailable) |
| `LEYLINE_SIGN_SECURITY_BIN` | `keystore.rs:634` | **removed** → `subprocess.securityBin` | path | `null` (`apple-password://` unavailable) |
| `LEYLINE_SIGN_HEALTHZ_PROBE_URL` | `host/health.rs:186` (name at `:84`) | **removed** → `health.probeUrl` | keystore URL | `null` (`?deep=1` reports `unconfigured`) |
| `KEYCHAIN_ACCOUNT` | `keystore.rs:494`, `helper.rs:291` | **removed** → `keychainAccount` | label | `"cloister"` |
| `LLO_TRUSTED_QUERY_HASHES` | `cmd_parse.rs:2900` | **removed** → `parser.trustedQueryHashes` | list of 64-hex, sorted | `[]` |
| `LLO_QUERY_MATCH_LIMIT` | `ts/src/query_engine.rs:116` | **removed** → `parser.queryMatchLimit` | integer 1-65536 | `4096` |
| `LLO_QUERY_PROGRESS_BUDGET` | `query_engine.rs:128` | **removed** → `parser.queryProgressBudget` | integer ≥ 1 | `100000` |
| `LLO_EMBEDDER` | `cli-lib/src/cmd_daemon.rs:540` (`vec` builds only) | **test seam**, behind `test-seams` (D3) | | no key: an embedder choice exists only in `vec` builds, and a build-dependent default would break D2's single `default` |
| `LLO_EXTRACTION_EPOCH` | `ts/src/refs.rs:76` | **test seam, kept** (D3): folded into `parserId`; mache's tests use it | | warning at start when set |
| `LLO_INJECTIONS_SCM` | `ts/src/injections.rs:233` | **test seam, kept** (D3): folded into the injection epoch and `parserId` | | warning at start when set |
| `LLO_DISABLE_INJECTIONS` | `ts/src/injections.rs:158` | **test seam**, behind `test-seams` (D3): changes facts without moving `parserId` | | absent from release builds |

`parser`, `head` and `diagnostics` appear in both daemon kinds (shared `$defs` in
`common.schema.json`), and `parse`, `verify-head` and `rebuild` read them from `--config` (D1).
`rootfs provision-cas` reads `stateDir`.

### Out of scope

Still read from the environment, as host conventions (D3): `HOME`, `XDG_STATE_HOME`, the
platform data directory, `RUST_LOG`; and for the one-shot `self`: `PATH`, `SHELL`,
`GITHUB_TOKEN` (`cmd_self.rs:108-114`); and the sign helper's Linux headless-keyring heuristic,
`DISPLAY`, `WAYLAND_DISPLAY` and `DBUS_SESSION_BUS_ADDRESS` (`helper.rs:445-452`), which
describe the session the helper runs in rather than configure it. Passed through, never read:
`OP_SERVICE_ACCOUNT_TOKEN`, `OP_SESSION_my`, `OP_ACCOUNT`, `OP_DEVICE`, `HOME` to the `op`
subprocess.

Build-time inputs (`env!`/`option_env!`, fixed into the binary, not runtime configuration):
`LEYLINE_BUILD_VERSION`, `LEYLINE_RELEASE_VERSION`, `LEYLINE_SIGN_BUILD_SHA`, `LLO_BUILD_DATE`.

Test-only (read only by `#[cfg(test)]` code or `tests/`): `LEYLINE_BENCH_FILES`,
`LEYLINE_BENCH_OUT`, `LEYLINE_BENCH_PROPOSALS`, `LEYLINE_BLESS_OCI_FIXTURE`,
`LEYLINE_CAS_VOLUME_TESTS`, `LEYLINE_CONFINEMENT_PROBE`, `LEYLINE_CONNECTIONS_RUNS`,
`LEYLINE_ENOSPC_CHILD_DIR`, `LEYLINE_EXECUTION_SOCKET`, `LEYLINE_GUEST_TOOLS`,
`LEYLINE_KRUN_WORKER`, `LEYLINE_LIBKRUNFW_PATH`, `LEYLINE_NONO_CHILD`,
`LEYLINE_NONO_DENIED_FILE`, `LEYLINE_NONO_ROOTFS`, `LEYLINE_NONO_RUNTIME_FILE`,
`LEYLINE_OCI_ACCEPTANCE_DIR`, `LEYLINE_OCI_NIX_CAS_DIR`, `LEYLINE_OCI_NIX_IMAGE`,
`LEYLINE_PROBE_CONNECT`, `LEYLINE_PROBE_DECOY`, `LEYLINE_PROBE_RUN_SIBLING`,
`LEYLINE_PROBE_SSH`, `LEYLINE_PROBE_STRICT`, `LEYLINE_PROBE_WAIT_FOR_GO`,
`LEYLINE_REGEN_SHEAF_REGION_VECTORS`, `LEYLINE_REGISTRY_NETWORK_TESTS`,
`LEYLINE_REQUIRE_HYPERVISOR`, `LEYLINE_REQUIRE_SANDBOX`, `LEYLINE_SHEAF_ABLATION_OUT_DIR`,
`LEYLINE_SIGN_KRUN_WORKER`, `LEYLINE_SOCKET`, `LEYLINE_TEST_VIRTIOFS_INIT_ENV`,
`LEYLINE_TEST_VIRTIOFS_INIT_MOUNTS`, `LEYLINE_TEST_VIRTIOFS_TOKEN`,
`LEYLINE_TEST_VIRTIOFS_VMM_ROOTFS`, `LEYLINE_WASM_ARTIFACT`, `LLO_CONNECT_PROBE`,
`LLO_PERF_GATES`, `LLO_TEST_CGROUP_ROOT`.

**Counts:** 46 flags (18 + 23 + 5), of which 44 map to keys, 1 is CLI-only (`--reset-arena`)
and 1 is removed (`--foreground`); one further key (`stateDir`) carries `--cas-volume`'s
optional value. 24 runtime environment variables, of which 19 map to keys (all removed and
refused when set), 1 becomes a flag of a one-shot command, 2 test seams stay (recorded in
`parserId`) and 2 move behind `test-seams`. 44 test-only and build-time
variables are out of scope. The inventory is reproducible:
`grep -rnoE '"(LEYLINE|LLO)_[A-Z0-9_]+"' rs --include='*.rs'`.

## Test plan

Vectors (schema-spec, `deployment-config/v1/test-vectors/`, pinned by `VECTORS.sha256`):

1. **Schema vectors.** Every accept case validates under its JSON Schema (`jsonschema` crate,
   already a schema-spec dev-dependency) and parses in Rust; every reject case the schema can
   express fails both, and every case it cannot (cross-field rules, `bad-string` escapes,
   duplicate keys) fails the Rust parser with the pinned field and reason.
2. **Unknown-key refusal** at the top level and in every nested object of every kind, including
   a key that differs only by case (`Kind`) and one from another kind.
3. **Discriminator:** `apiVersion` and `kind` each absent, of the wrong type, unknown
   (`config.leyline/v2`, `FooConfig`), another process's kind (`wrong-kind`), and both placed
   last in the text (still checked first, `apiVersion` before `kind`).
4. **Defaults resolution:** the minimal document of each kind (only `apiVersion` and `kind`,
   plus the four required execution fields) resolves to pinned bytes and `configDigest` for
   `HOME=/Users/fixture` on macOS and `HOME=/home/fixture` on Linux, with and without
   `XDG_STATE_HOME`; `verify_deployment_config_defaults` checks every schema `default` against
   the resolver. `null` accepted where the default is `null`, refused elsewhere.
5. **Canonical form:** shuffled key order, unsorted set lists and `~/` paths resolve to the same
   bytes; duplicate set entries refused; `canonical-strings.json` pins non-ASCII paths, a list
   whose UTF-8 order differs from its UTF-16 order (U+FF61 and U+1F600), empty lists and
   objects, and refuses U+2028, U+007F and a C1 control; a non-canonical IPv4 or IPv6 spelling
   is refused; an integer above 2^53 − 1 is refused.
6. **Second implementation:** `ref-impl-py` reaches every pinned canonical byte string and
   `configDigest` from each vector's resolved document (`verify_deployment_config_digest`).
7. **Secret refusal:** a string, a number, an array and `{"value": …}` in each secret field are
   refused with `secret-inlined` and the field's name; `--print-config` output contains the
   reference and not the file's bytes (asserted by searching the output for the fixture secret).
8. **TOML/JSON equivalence:** each TOML case and its JSON twin produce identical resolved bytes
   and digest; a TOML datetime and a float are refused; an empty TOML table for a union
   (`[backend]`, `[evidence]`, `[cas]`) and for an object with a required key (`[mount]`,
   `[mcp.http]`) is refused; an unknown extension is refused.
9. **Fail-closed default gate:** a published-schema fixture whose `x-leyline-fail-closed`
   default loosened under the same major version fails the check.
9a. **Reserved secret variants:** `{"keychain": …}` and `{"vault": …}` in each secret field are
   refused as `secret-ref-not-delivered` naming the variant; `{"other": …}` is `unknown-key`.
9b. **Library entry** (`leyline-runtime`): every `ExecutionDaemonConfig` accept case run through
   `resolve_execution_deployment` yields the pinned resolved bytes, `configDigest` and backend
   struct values (`worker`, `cas_root`, `ephemeral_root`, `runtime_files`, `devices`,
   `ready_timeout`, `tsi_hijack_inet`), with an injected `HostFacts`; the daemon's `--config`
   path and this test call the same function. `check_removed_environment` with three removed
   variables returns all three with their keys; its list equals
   `leyline-removed-variables.json`, which equals the generated file (drift gate).
9c. **Symlinked directories:** a secret file and a config file under `/tmp` on macOS (a symlink
   to `/private/tmp`) are checked at the resolved path and accepted when the resolved
   directories pass; a parent directory replaced by a symlink between the check and the read is
   not followed (the read is `openat` on the resolved directory with `O_NOFOLLOW`).

CLI tests (`rs/ll-open/cli/tests/deployment_config.rs`):

10. **Precedence:** `--config` with any configuration flag exits 64 naming the flag;
    `--config --reset-arena` and `--config --expect-config-digest` are accepted; for each kind,
    a flag set and the document the adapter should produce give the same `--print-config`
    bytes (a table of flag sets, including mache's spawn argv and cloister's e2e-smoke argv).
11. **Flag adapter:** every pair D10 lists exits 64: the inert ones (`--nfs-port` without
    `--mount`, `--mcp-bind` without `--mcp-port`, `--tsi-hijack-inet` or a `--workspace-*-ms`
    with `--backend native`) and every cross-branch pair (`--allow-unverified-evidence` with
    `--trust-key`, `--device` with native, `--cgroup-root` with micro-vm,
    `--cas-volume-size-cap-gib` with `--cas-root`); `--timeout 0s` resolves to `0`;
    `a_partial_or_invalid_verifier_configuration_refuses_startup` passes unchanged.
12. **Removed variables:** for every subcommand of both binaries, each removed variable set
    refuses, naming it and its key, with status 78, within one second, and before any socket,
    lock, arena or control file exists (asserted by checking the configured paths afterwards);
    with two set, both are named; unset, they run. `LLO_EXTRACTION_EPOCH` set starts with a
    warning and moves `parserId`. A release-build smoke test sets `LLO_DISABLE_INJECTIONS` and
    asserts output and `parserId` are unchanged.
13. **`--check-config` and `--print-config` write nothing:** snapshot `HOME`, the state
    directory, the data directory and the run root (names, modes, mtimes) before and after,
    for each kind, with the run root both missing and existing, and with the token file
    missing; they are identical, and deferred checks are listed on stderr.
14. **Secret and document files:** a secret file with mode `0644`, owned by another user, that
    is a symlink, or in a group-writable directory is refused (`secret-file-permissions`); a
    planted symlink at a missing `mcp.tokenFile` is never written through; an empty existing
    token file is refused; a `--config` file that is group-writable, owned by another user or a
    symlink is refused (`config-file-permissions`, exit 78).
15. **Exit codes:** 0, 64, 65, 66 and 78 each produced by a fixture; stderr lines parse as
    JSON with `field` and `reason`.
16. **The standalone example** (D8).
17. **Fail-closed defaults:** the minimal execution document without `evidence` exits 65
    (and `without_a_verifier_configuration_every_start_is_refused` is rewritten to pin it); a
    daemon document with `mcp.http.bind = "0.0.0.0"` and no `allowPublic` exits 65; a
    sign-helper document with no `auth` exits 65, and one whose caller-tokens file holds `""`,
    `"\n"` or `",,"` is refused at `--check-config` and at start (`caller-tokens-required`); a
    run root inside or equal to the CAS root is refused under both `cas` branches; each
    loosening field produces its stderr notice.
18. **Startup log and pin:** the daemon logs the same `configDigest` `--check-config` printed;
    `--expect-config-digest` with a different digest exits 78 before binding anything.
19. **Published schema:** `--print-config-schema` equals the checked-in published file, and
    `test_release_publication.sh` asserts each release asset equals it, and that
    `leyline-removed-variables.json` is published.
20. **`configDigest` on the wire:** the daemon's `status` reply carries the digest
    `--print-config` reports; a run's `RunReceipt.configDigest` (`@17`) and its `RunOutcome`
    carry it; a backend built by hand carries none; `task compat:check` and
    `task schema:consumer-fixture-test` pass with the regenerated clients.
21. **Defaults that moved:** `worker.readyTimeoutMs` resolves to 60000 and `--ready-timeout-ms`
    absent gives the same; the sign helper's minimal document resolves `auth.required` and
    `signAllow.required` to `true`.
22. **`mount` in releases:** the release workflow's staged `leyline` reports the NFS mount
    backend in `leyline capabilities`, checked by the release gate before publication.

## Implementation slice for v0.30.0

Two pull requests, both in v0.30.0; the second depends on the first's schema-spec module.

**PR 1: the document, both leyline daemons, environment removal.**

| Area | Files | Rough lines |
|---|---|---|
| Spec | `schema-spec/deployment-config/v1/` README, `common` + 2 kind schemas, vectors (including `canonical-strings.json`), `VECTORS.sha256` | ~500 README, ~500 schema JSON, ~1,000 vector JSON |
| Reference implementation | `deployment-config/v1/ref-impl-py/deployment_config_v1.py` (standard library only, own reader, writer and BLAKE3, following `connections/v1/ref-impl-py`) | ~450 |
| Parser, resolver, canonical writer, digest, TOML adapter, file checks (no-follow open, owner and mode, parent directory) | `schema-spec/src/deployment_config.rs` (+ `toml` dependency, already locked) | ~1,200 |
| Spec tests | `schema-spec/tests/verify_deployment_config_*.rs` (vectors, defaults, digest via the reference implementation, fail-closed default gate, published-schema drift) | ~550 |
| CLI | `cli/src/main.rs`: `--config`, `--check-config`, `--print-config`, `--print-config-schema`, `--expect-config-digest`, flags→document adapter (`value_source`), document→`DaemonConfig`/backend mapping, read-only host checks split from start's writing steps, exit codes | ~750 changed |
| Settings plumbing | `cli-lib`: `head_claim.rs`, `cmd_parse.rs` (signing key from a file, trusted keys, trusted query hashes), `cmd_rebuild.rs`, `daemon/ops.rs`, `daemon/auth.rs` (exclusive no-follow creation, empty file refused), `session_observation_pass.rs`, `sheaf_ablation.rs`, `cmd_daemon.rs`, `cmd_rootfs.rs`, `cmd_self.rs`; `ts`: `query_engine.rs`, `injections.rs` (`test-seams` for `LLO_DISABLE_INJECTIONS`); `runtime/cas_volume.rs` | ~600 |
| Published schema and removed-variable list | generator task `gen:deployment-config-schema`, checked-in `deployment-config/v1/published/` (three schemas and `leyline-removed-variables.json`), `include_str!`, `tools/stage_release_artifacts.sh`, `release.yml`, `tools/test_release_publication.sh`, `tools/release-assets.txt` | ~170 + generated JSON |
| Library entry | `runtime/src/deployment.rs` (`HostFacts`, `resolve_execution_deployment`, `check_execution_host`, `check_removed_environment`, `REMOVED_VARIABLES_JSON`), its vector test; `cli/src/main.rs` builds the execution daemon from it | ~450 |
| `configDigest` on the wire | `public-schema/capnp/daemon.capnp` (`StatusResponse.configDigest @13`), `execution/v1/execution.capnp` (`RunReceipt.configDigest @17`), `runtime/src/model.rs` (`RunOutcome.config_digest`), receipt producer; regenerated Go/TypeScript clients, `execution.tools.json`, `compatibility.json`, consumer fixtures, receipt vectors | ~250 + generated |
| `mount` in releases | `.github/workflows/release.yml` (`--features mount` for `leyline` on every target), `tools/stage_release_artifacts.sh` / the release gate (staged binary reports the mount backend), `README.md` and `GETTING-STARTED.md` (release binaries mount) | ~60 |
| Removed-variable refusal and seam warnings | `cli-lib` (before dispatch, through `check_removed_environment`) | ~60 |
| CLI tests | `cli/tests/deployment_config.rs`; rewrite `execution_evidence.rs:374`; tests that set removed variables move to documents (`cache/tests/common/mod.rs`, `cli-lib/tests/{arena_query_blobs_test,injection_extraction_test,markdown_inline_injection_test,head_policy_env}.rs`, `cli/tests/daemon_startup.rs`); `f6_extraction_epoch_invalidation.rs` and `f7_injection_epoch_invalidation.rs` keep their seams | ~800 |
| Docs | `docs/examples/deployment-config/` (3 files), `GETTING-STARTED.md`, `AGENTS.md` vocabulary row, `changes/52c177-*.md` (Breaking, Removed, Changed) | ~150 |

**PR 2: the sign helper.** `sign-helper-config.schema.json` + vectors (~300 JSON), its module
in `deployment_config.rs` (~250), `sign/src/bin/helper.rs`, `host/keystore.rs`,
`host/health.rs` read a settings struct instead of the environment, every refusal before
binding (~350), tests (~300). Lands with the cloister sign-helper bead (D12).

Total: about 5,400 lines of Rust, Python and tests, and 2,100 lines of JSON and prose, plus
regenerated bindings. Gates: `task check`, `task compat:check`, `task gen:server-json:check`,
`task schema:consumer-fixture-test`, `task runtime:test`, `task rs:mutants:pr`, `task ci`, and
a release dry run for the `mount` and asset changes. The receipt and status fields are wire
changes, so their consumer beads (cloister reads `RunOutcome`; mache reads `status`) are named
on the implementation PR before merge; neither consumer must act, since both fields are
additive and optional.

## Review record

**LLO specialist reviews of the proposed draft** (`cross-runtime-contract-auditor`,
`execution-boundary-adversary`): every in-scope finding is folded into the draft at `06cf7501`
(summarized on bead `ley-line-open-52c177`).

**Consumer reviews of `06cf7501`, and their dispositions:**

| Item | Disposition |
|---|---|
| cloister F1 (blocking): cloister links `leyline-runtime` in-process; inventory was read from a stale branch | Accepted: D13 adds the public library entry `resolve_execution_deployment`, tested by the vectors and used by the daemon; D11 is redone against cloister `main` `80ef046`. |
| cloister F2: what the library does about removed variables; publish the list as data | Accepted: D13 `check_removed_environment` (embedders call it); D2 publishes `leyline-removed-variables.json`, generated and drift-gated. |
| cloister F3: does the directory check follow symlinks | Accepted: D4 `lstat`s the file, `realpath`s the directory, checks every resolved directory, reads through `openat` with `O_NOFOLLOW`. |
| cloister F4: 5 s ready timeout too low | Accepted: D5 sets 60000, from the live suites' and cloister's 30 s and readiness above 30 s under load. |
| cloister F5: keep `secretRef` extensible | Accepted: D4 reserves `keychain` and `vault`, refused by name in v1; D9 makes delivering one additive. |
| cloister F6: `--expect-config-digest` | Already in the draft (D5); kept. |
| cloister F7: `configDigest` in the receipt and `RunOutcome` | Accepted with Q6: D6 adds `RunReceipt.configDigest @17` (after Slice 1's `@15`/`@16`, #576), `RunOutcome.config_digest` and `StatusResponse.configDigest @13`. |
| cloister F8: broker Slice 1 knobs | Decided: constants in v1, not keys (D5), following ADR-0046 S-Q2 and S-Q3. |
| mache (`mache-0be425`): surface, not scrub; name each variable; exit before socket work | Accepted: D3 exits 78 before any socket, lock, arena or control file, naming every offending variable with its replacement; `LLO_EXTRACTION_EPOCH` stays a test seam. |

No open questions remain. Two later decisions are named, not deferred work: removing the
daemon flags once no consumer spawns with them (D10), and delivering a reserved `secretRef`
variant (D4).
