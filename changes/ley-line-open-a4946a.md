Section: Fixed

- **The release bump and the release verifier read one public schema
  surface** (bead `ley-line-open-a4946a`). `tools/bump_release_version.sh`
  decided whether `SCHEMA_VERSION` moves from `clients/go/leyline-schema`
  alone, while `tools/verify_release_version.sh` also refused a release whose
  `rs/ll-core/schema-spec` IDL or vectors had changed, so v0.27.0 and v0.28.0
  (schema-spec vector changes only) were bumped into a state the verifier
  refused and were fixed by hand. The bump, the verifier and
  `tools/tag_release.sh` now decide through `release_schema_surface_unchanged`
  in `tools/release_common.sh`: the Go client plus the schema-spec Cap'n
  Proto IDL, JSON Schema IDL and conformance vectors. `tag_release.sh`, which
  checked only the Go client, now refuses schema-spec drift too.
  `task release:schema-surface:fixture-test` (run by `task ci`) proves a
  vector, IDL or Go change raises `SCHEMA_VERSION` and verifies, a change
  outside the surface leaves it and verifies, and a held-back surface change
  is refused.
