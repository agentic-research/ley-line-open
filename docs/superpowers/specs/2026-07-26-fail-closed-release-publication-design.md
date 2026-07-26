# Fail-Closed Release Publication Design

Status: approved
Bead: `ley-line-open-9bfd98`

## Goal

Make one Taskfile command publish a release without allowing an unverified
payload to cross the GitHub release boundary. Keep the binary, public schema,
and wire-format versions independent and explicit.

## Version domains

The repository publishes two independently versioned works:

- The `leyline` binary and `leyline-fs` artifacts are released from the
  AGPL-3.0-or-later repository root as `vX.Y.Z`.
- `clients/go/leyline-schema` is an Apache-2.0 nested Go module released as
  `clients/go/leyline-schema/vX.Y.Z`.

The nested module contains its own Apache-2.0 `LICENSE`; the release gate must
verify that the source tree and the downloaded public module both retain that
license boundary.

A binary-only storage change does not create a schema version. For the CDC
integrity work, the compatibility pair is:

```text
binary: v0.10.4
schema: v0.10.3
wire-format major: 1
```

The publication command reads the canonical schema version and verifies its
existing public nested tag. It creates a new nested tag only when the schema
version has advanced.

## Command shape

`task release:publish VERSION=0.10.4` is the sole human entrypoint. It composes
four independently testable phases:

1. `release:preflight` verifies a clean checkout, an immutable source commit,
   version alignment, schema compatibility, license packaging, required tools,
   and absence of conflicting remote tags.
2. `release:tag` creates the root tag and, only when required, the nested schema
   tag. Multiple new refs are pushed atomically.
3. The trusted release workflow builds target artifacts without write
   permission, stages per-target manifests, and passes them to one publisher.
4. `release:verify-public` downloads the public release, requires the exact
   asset set, verifies the aggregate manifest, and proves public Go-module
   resolution for the declared schema version.

The outer command waits for the workflow and returns success only after the
public postflight succeeds.

## Manifest contract

Per-target staging manifests are internal workflow evidence. The publisher:

1. verifies every per-target manifest while artifacts remain in separate
   directories;
2. rejects duplicate payload names;
3. flattens only verified payloads into a fresh publication directory;
4. generates one aggregate `SHA256SUMS` over payloads only;
5. verifies that aggregate manifest; and
6. uploads the payloads and `SHA256SUMS` together.

`SHA256SUMS` never contains an entry for itself. Verification compares the
manifest entries with the exact set of non-manifest files.

The GitHub release object and upload step occur only after all build artifacts
pass verification. Shell entrypoints use fail-fast execution, and workflow job
dependencies make the credentialed publisher unreachable after any failed
build or verifier.

## Historical recovery

`v0.10.3` is not rebuilt or retagged. Recovery downloads its eight existing
assets, verifies their independently recorded digests, creates the aggregate
non-self-referential manifest, uploads only that missing manifest, downloads
the public release again, and runs the same public postflight. The source and
nested schema tags must remain at
`a4f57673f0f79d0e3dd8808f19a8b6fc9c5b3347`.

## Falsification

Fixture tests must prove:

- a manifest that lists itself is rejected;
- a missing, extra, corrupt, or duplicate payload is rejected;
- upload is not invoked after any verifier failure;
- unchanged schema versions do not create new nested tags;
- changed schema versions require an atomic matching nested tag;
- a root/nested tag pointing at the wrong commit is rejected;
- the public verifier rejects a release without `SHA256SUMS`;
- the downloaded nested module contains the Apache-2.0 license; and
- the successful fixture publishes the exact expected payload set once.

No test may contact GitHub or mutate real tags. External commands are injected
or replaced with fixture executables for publication-path tests.
