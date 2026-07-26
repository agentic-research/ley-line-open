# Fail-Closed v0.10.4 Publication Implementation Plan

Beads: `ley-line-open-9bfd98`, `ley-line-open-efec2d`,
`ley-line-open-ed37c9`

## Outcome

One `task release:publish VERSION=0.10.4` operation must atomically publish
the root and nested schema tags, wait for a build-only matrix, allow exactly
one credentialed publisher to create the release only after local artifact
verification, and return success only after public assets and the public Go
module verify. The same verifier must repair and prove the existing v0.10.3
release without rebuilding it.

## Task 1: Make the schema consumer contract executable

1. Add a hermetic external-module fixture that imports
   `clients/go/leyline-schema/daemon/wire`, compiles representative response
   and event usage, and checks the module-local Apache-2.0 license.
2. Run it against the current tree to prove the canonical package, rather than
   a duplicate wrapper, is the supported API.
3. Advance the binary crates, public schema crates, daemon
   `SCHEMA_VERSION`, compatibility metadata, README, and changelog to the
   explicit v0.10.4 compatibility point.
4. Add version and documentation gates so the root release, nested tag, and
   advertised consumer API cannot drift independently.

## Task 2: Build one aggregate publication set

1. Add an authoritative expected-asset list.
2. Add a preparer that first verifies every per-target manifest, rejects
   duplicate names, copies only verified payloads into a fresh directory, and
   generates a sorted aggregate `SHA256SUMS` that excludes itself.
3. Add a public-set verifier that requires the exact expected filenames,
   rejects malformed, duplicate, self-referential, missing, extra, and corrupt
   entries, and verifies every digest.
4. Extend fixture tests with one success case and a mutation matrix covering
   every rejection path.

## Task 3: Make publication fail closed

1. Remove the early credentialed release-creation job from `release.yml`.
2. Keep build jobs read-only and make the sole write-capable publisher depend
   on every matrix build.
3. In that publisher, prepare and verify the aggregate directory before the
   first `gh release` mutation; then create the release idempotently and upload
   the exact aggregate set once.
4. Add a structural workflow gate and fake-command fixture proving no release
   creation or upload is reachable after a failed verifier.

## Task 4: Expose one Taskfile release operation

1. Add a preflight that validates `VERSION`, a clean immutable checkout,
   binary/schema/version metadata, expected tools, and remote tag state.
2. If the schema tag is new, require its version to equal the root release and
   push root plus nested refs atomically; if it already exists, verify it and
   do not recreate it.
3. Find and watch the exact tag-triggered workflow run.
4. Download the public release into a fresh directory, verify the aggregate
   assets, resolve and compile the public nested Go module, and require its
   Apache-2.0 license.
5. Exercise preflight and orchestration against fake `git`/`gh` commands so
   real tags and releases are never mutated by fixtures.

## Task 5: Recover and release

1. Download the eight existing v0.10.3 assets, independently compare them
   with their recorded digests, generate the non-self-referential aggregate
   manifest, upload only `SHA256SUMS`, redownload, and run the public verifier.
2. Confirm both v0.10.3 tags still peel to
   `a4f57673f0f79d0e3dd8808f19a8b6fc9c5b3347`.
3. Run focused tests, `task fmt`, full `task ci`, and an adversarial
   operation-layer review.
4. Commit and push one PR whose title/body describes the actual integrity
   hole, version contract, failure boundary, performance cost, exact test
   evidence, and the three beads.
5. Merge only after required checks pass, then run the single v0.10.4
   publication operation and independently verify both public tags, assets,
   checksums, module imports, and license.
