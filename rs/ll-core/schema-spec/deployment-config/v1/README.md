# deployment-config/v1 — `apiVersion: config.leyline/v1`

**Status:** Shipped in v0.30.0 (ADR-0049, bead `ley-line-open-52c177`).
**Implementation:** `leyline_schema_spec::deployment_config` (parser, resolver,
canonical writer, digest, file rules, removed-variable check);
`leyline_runtime::deployment::resolve_execution_deployment` (the execution
document mapped into the structs the execution daemon builds).
**Second implementation:** `ref-impl-py/deployment_config_v1.py` (standard
library only: its own reader, canonical writer and BLAKE3).

Each LLO daemon kind is configured by one typed, versioned JSON document.
JSON is the native form; TOML is an adapter onto the same data model. Every
optional field has a default stated in the schema, so an omitted field means
LLO's default, never "unset". The document configures mechanism and carries
no per-run authority (ADR-0035).

## Files

| File | Role |
|---|---|
| `common.schema.json` | Shared `$defs`: `path`, `label`, `ipAddress`, `queryHash`, `urlPrefix`, `secretRef`, `source`, `parser`, `head`, `diagnostics`, `enrichment` |
| `daemon-config.schema.json` | `DaemonConfig` (`leyline daemon`) |
| `execution-daemon-config.schema.json` | `ExecutionDaemonConfig` (`leyline execution-daemon`, and the runtime library entry) |
| `sign-helper-config.schema.json` | `SignHelperConfig` (`leyline-sign-helper`) |
| `removed-variables.source.json` | The environment variables v0.30.0 removed, each with its replacement key (normative) |
| `fail-closed-defaults.json` | The baseline of every `x-leyline-fail-closed` default (D9 gate) |
| `published/` | Generated, checked in, drift-gated, embedded and released: one self-contained schema per kind (`leyline-<kind>.schema.json`) and `leyline-removed-variables.json`. Regenerate with `task gen:deployment-config-schema`. Consumers compare these by bytes and pin each by SHA-256 (file integrity, not an LLO identity domain). |
| `test-vectors/` | Accept and reject cases per kind, `canonical-strings.json`, `toml-equivalence.json`; pinned by `VECTORS.sha256` |

## Kinds and the discriminator

| `kind` | Process |
|---|---|
| `DaemonConfig` | `leyline daemon` |
| `ExecutionDaemonConfig` | `leyline execution-daemon` |
| `SignHelperConfig` | `leyline-sign-helper` |

One file holds exactly one document. Two keys are checked before anything
else, wherever they appear in the text, in this order:

1. `apiVersion`: absent `missing-key`; not a string `wrong-type`; any value
   other than `config.leyline/v1` `bad-api-version`.
2. `kind`: absent `missing-key`; not a string `wrong-type`; no such kind
   `unknown-kind`; a kind for another process `wrong-kind`.

`parse`, `verify-head` and `rebuild` accept either daemon kind and read only
`parser`, `head` and `diagnostics`; `rootfs provision-cas` accepts only
`ExecutionDaemonConfig` and reads only `stateDir`.

## Rules

Checks run in this order, and the first refusal is reported, so two runs
report the same one:

1. **Syntax.** JSON (RFC 8259), no byte-order mark, nesting at most 16
   (`malformed`). For TOML, the adapter's own refusals (below) happen here.
2. **Discriminator**, as above.
3. **Fields**, in the order the schema lists them (`properties` order of
   each source schema). Within one object, keys are first scanned in input
   order: a key twice is `duplicate-key`, an undefined key (including one
   that differs only by case, or one from another kind) is `unknown-key`.
4. **Cross-field rules**, in the order listed under each kind below.

Field rules:

- **Strings** are UTF-8 and contain none of U+0000–U+001F, U+007F–U+009F,
  U+2028, U+2029, `"` or `\` (`bad-string`). A JSON escape is refused even
  when it decodes to an allowed character: the canonical writer then needs
  no escape.
- **Integers** are JSON integers with no fraction or exponent
  (`not-an-integer`), non-negative and within the field's range
  (`out-of-range`); no maximum exceeds 2^53 − 1. No field is a float.
- **`null`** is accepted only where the default is `null`, and means the
  same as omitting the field. Elsewhere it is `null-not-allowed`.
- **Required** fields: `cas`, `worker` (with `worker.path`), `catalog` and
  `evidence` in `ExecutionDaemonConfig`; inside a present optional object,
  `mount.path`, `source.path`, `mcp.http.port`, `evidence.verified.trustKeys`
  and `evidence.verified.store`. Absent: `missing-key`.
- **Unions** (`cas`, `backend`, `evidence`) hold exactly one key: `{}` (an
  empty TOML table) is `union-empty`, two keys `union-ambiguous`.
- **Enums**: a value outside the closed set is `not-in-enum`.
- **Paths** are absolute, or begin `~/`, which expands against `HOME`
  (only `~/`: `~user` and relative paths are `path-not-absolute`). No
  empty, `.` or `..` component and no trailing `/` (`bad-path`).
- **Labels** (`source.language`, `mcp.groups`, `signAllow.callers` keys,
  `keychainAccount`): 1–64 of `[A-Za-z0-9._-]`, starting with a letter or
  digit (`bad-label`).
- **Query hashes**: 64 lowercase hex characters (`bad-query-hash`).
- **URL prefixes** (`resolveAllow`, `signAllow` prefixes, `health.probeUrl`):
  a lowercase scheme, `://`, then printable ASCII without space, `"` or `\`,
  at most 2048 bytes (`bad-url-prefix`).
- **IP literals** have one spelling: IPv4 a dotted quad with no leading
  zero, IPv6 RFC 5952 text. Another spelling of a valid address is
  `non-canonical-address`; a non-address `bad-address`. The sign helper's
  `bind` is `127.x.y.z:port` or `[::1]:port`, port 1–65535 with no leading
  zero; a non-loopback address is `bind-not-loopback`.
- **Sets** (`trustKeys`, `head.trustedKeys`, `devices`, `runtimeFiles`,
  `mcp.groups`, `parser.trustedQueryHashes`, `resolveAllow`, each
  `signAllow.callers` list) are sorted in the resolved document; an entry
  twice, after `~/` expansion, is `duplicate-entry`, never merged.
- **Secrets** (`head.signingKey`, `mcp.tokenFile`, `auth.callerTokens`) take
  a reference: `{"file": <path>}`. A string, number, boolean, array, or an
  object with key `value` is `secret-inlined`. `keychain` and `vault` are
  reserved variants, refused by name as `secret-ref-not-delivered`; any
  other key is `unknown-key`. Delivering a reserved variant later is an
  additive v1 change.

### Cross-field rules

- `DaemonConfig`: a non-loopback `mcp.http.bind` without
  `mcp.http.allowPublic: true` is `public-bind-without-opt-in`. The binary
  then refuses an `mcp.groups` entry it has no group for
  (`unknown-mcp-group`).
- `ExecutionDaemonConfig`: the run root is neither inside, a parent of, nor
  equal to the CAS root, under both `cas` branches
  (`run-root-overlaps-cas-root`; under `cas.volume` the CAS root is
  `<stateDir>/cas-volume/mnt/cas` on macOS, `<stateDir>/cas` on Linux). Then
  `backend.native.cgroupRoot` on a host that is not Linux is
  `cgroup-root-linux-only`.
- `SignHelperConfig`: `auth.required` with no `auth.callerTokens` is
  `caller-tokens-required` (a named file that holds no caller is refused the
  same way when it is read); `signAllow.required` with no caller holding a
  prefix is `sign-allow-required`. The binary then refuses a `resolveAllow`
  prefix that would authorize a signing-key URL
  (`resolve-allow-signing-key`).

## Host defaults

Resolution reads `HOME`, `XDG_STATE_HOME`, the platform data directory and
the OS, injected as `HostFacts`, and nothing else. A value is checked only
when a default needs it; one that is unset, relative, not UTF-8, holds a
refused character or a bad component is `bad-host-default` for the field
that needed it, never replaced.

| Field | Default |
|---|---|
| `arena.path` | `<HOME>/.mache/default.arena` (`DaemonConfig`), `<HOME>/.mache/execution.arena` (`ExecutionDaemonConfig`) |
| `control` | `arena.path` with its extension replaced by `.ctrl` |
| `mount.backend` | `nfs` on macOS, `fuse` on Linux |
| `mcp.tokenFile` | `{"file": "<data_dir>/leyline/daemon.token"}` |
| `stateDir` | `<XDG_STATE_HOME>/leyline` when `XDG_STATE_HOME` is set and absolute, else `<HOME>/.local/state/leyline` |
| `cas.volume.runRoot` | `<stateDir>/cas-volume/mnt/runs` on macOS, `<stateDir>/runs` on Linux |

Every other default is in the schema's `default` keyword;
`verify_deployment_config_defaults` checks the resolver agrees with each.
Defaults that refuse or narrow stay so: `evidence` has none, `mcp.http.bind`
is `127.0.0.1`, `mcp.noAuth`, `mcp.http.allowPublic` and
`backend.microVm.tsiHijackInet` are `false`, and the sign helper's
`auth.required` and `signAllow.required` are `true`. `worker.readyTimeoutMs`
is 60000.

## Canonical form and `configDigest`

The resolved document has every key present (defaults filled, `~/`
expanded, `null` written, not omitted). Its canonical bytes follow
`confinement/v1` §7 rules 1–4 as `connections/v1` does, with four
differences:

1. `null` is written.
2. Strings are UTF-8 (minus the refused characters), written raw with no
   escape.
3. Keys and set lists sort by UTF-8 bytes, which is Unicode code point
   order, not UTF-16 code unit order (`canonical-strings.json` pins a list
   that tells them apart).
4. Set lists are sorted; a one-element list stays a list; an empty list is
   `[]` and an empty object `{}`.

Concretely: UTF-8 with no BOM; keys sorted at every level; two-space
indentation; `": "` between key and value; `,` then a newline between
members; no trailing whitespace and no trailing newline. Integers in
decimal with no sign, fraction, exponent or leading zero. This is Python's
`json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False)` of the
resolved value with set lists sorted.

`configDigest` is BLAKE3-256 over those bytes, written `blake3-256:<hex>`.
It names one resolved configuration of one LLO release on one host. It is
not an authority: never signed by LLO on its own, never carried in a grant
or a head, and nothing is admitted or refused by comparing it, except
`--expect-config-digest`, which compares a launcher's own earlier result.
Although written like a CAS address, it is never resolved against a CAS. It
covers the document only, not the files the document names.

## TOML adapter

Chosen by the `.toml` extension (`.json` is JSON; any other extension is
`unknown-config-syntax`). Tables become objects, arrays arrays, strings and
booleans map directly, integers keep their value. A float or datetime is
`toml-type-not-mapped`, naming its field. TOML has no `null`; omit the key.
TOML's duplicate-key rule refuses a repeat as `malformed`. A TOML document
and its JSON twin have the same resolved bytes and `configDigest`.

## File rules (D4)

A secret file must be a regular file, not a symlink, owned by the process's
user, mode `0600` or `0400`; the `--config` file the same, except that it
may be readable by group and others (`0644`, `0444`). The parent directory
is resolved with `realpath`, then opened from `/` one component at a time
with `openat(O_DIRECTORY | O_NOFOLLOW)`; each directory, checked with `fstat`
on its descriptor, is owned by root or the process's user and has no group
or other write bit unless its sticky bit is set (as on `/private/tmp`). The
file is opened relative to the last directory with `O_NOFOLLOW`, checked
with `fstat` and read through that descriptor, so what was checked is what
is read. Refusals: `secret-file-permissions`, `config-file-permissions`;
an empty existing secret is `secret-file-invalid`; a missing `--config`
file is `unreadable`. `mcp.tokenFile` is generated when absent, created
with `O_CREAT | O_EXCL | O_NOFOLLOW` and mode `0600`, so a planted symlink is
never written through.

## Removed environment variables

`published/leyline-removed-variables.json` lists every variable v0.30.0
removed, sorted by name, with its replacement key. A process that finds one
set (whatever its value) refuses at start with exit 78, before any socket,
lock, arena or control file, naming every offender on stderr:

```text
leyline: refusing to start: removed environment variables are set (ADR-0049):
  LEYLINE_PROFILE: use diagnostics.profile in the config document
  LLO_TRUSTED_QUERY_HASHES: use parser.trustedQueryHashes in the config document
```

`check_removed_environment` is the same check for embedders.
`LLO_EXTRACTION_EPOCH` and `LLO_INJECTIONS_SCM` are test seams, not
configuration: both are folded into `Head.parserId`, are not refused, and a
process that sees one set logs a warning naming it.

## Refusals

Every refusal is `{"field": <dotted, empty for the document>, "reason":
<code>, "message": <text>}`; codes are stable within v1. Exit statuses:

| Exit | Codes |
|---|---|
| 65 (document refused) | `malformed`, `duplicate-key`, `bad-string`, `wrong-type`, `missing-key`, `unknown-key`, `bad-api-version`, `unknown-kind`, `wrong-kind`, `out-of-range`, `not-an-integer`, `non-canonical-address`, `bad-address`, `null-not-allowed`, `union-empty`, `union-ambiguous`, `not-in-enum`, `path-not-absolute`, `bad-path`, `duplicate-entry`, `secret-inlined`, `secret-ref-not-delivered`, `bad-label`, `bad-query-hash`, `bad-url-prefix`, `public-bind-without-opt-in`, `unknown-mcp-group`, `run-root-overlaps-cas-root`, `caller-tokens-required`, `sign-allow-required`, `bind-not-loopback`, `resolve-allow-signing-key`, `toml-type-not-mapped`, `unknown-config-syntax` |
| 66 (file unreadable) | `unreadable` |
| 78 (valid, but this host or binary cannot run it) | `cgroup-root-linux-only`, `bad-host-default`, `unsupported-os`, `secret-file-permissions`, `secret-file-invalid`, `config-file-permissions`, `config-digest-mismatch`, `mount-feature-not-built`, `fuse-feature-not-built`, `host-check-failed` |

## Versioning

Within v1, adding an optional field with a default is additive (an older
binary refuses the new key, failing closed). Narrowing a default is a
Breaking CHANGELOG entry and updates the pinned vectors and
`fail-closed-defaults.json`. Loosening any `x-leyline-fail-closed` default,
and removing, renaming, retyping or changing the meaning of a field, is
`config.leyline/v2` for that kind; a binary reads one version per kind.
