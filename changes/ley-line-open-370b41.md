Section: Added

- **`leyline_runtime::artifacts::oci::Platform` exposes the rule `import-oci` selects a platform with** (bead `ley-line-open-370b41`), so a consumer such as Cloister can pick the lock row this host boots without mirroring the rule.
  - `Platform: FromStr` parses `os/architecture[/variant]` and refuses anything else by name.
  - `Platform::admits(&Platform)` decides whether this platform runs an image for the candidate platform:
    - `os` and `architecture` must match, by OCI's names (`amd64`, never `x86_64`);
    - a platform without a variant admits any variant;
    - `arm64` without a variant is `arm64/v8`;
    - an empty variant is no variant.
  - `Platform::select(&[candidates])` returns the index of the one platform string this platform runs. It refuses none or several as the new `PlatformError::{NoMatch, Ambiguous}`, naming the candidates, and refuses a malformed string as `PlatformError::Malformed`.
  - An image config or index entry with an empty `variant` is now treated as having none.
