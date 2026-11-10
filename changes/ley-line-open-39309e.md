Section: Added

- **The rootfs digest is in ADR-0047's identity inventory, with its
  canonical bytes and a known-answer vector** (bead `ley-line-open-39309e`).
  ADR-0047 §3 gains the rootfs digest row: BLAKE3-256 over the exact bytes
  of `rootfs.manifest.json`, whose canonical form (compact JSON, key order,
  decimal permission bits, lowercase hex, entries in component-wise path
  order) is stated there. `execution/v1/test-vectors/rootfs-manifest.json`
  publishes the checked-in OCI fixture's linux/arm64 manifest bytes and
  digest; `leyline-runtime`'s `oci_rootfs` test checks the digest and that
  the importer writes exactly those bytes.
