Section: Added

- **`leyline_runtime::artifacts::oci::Platform` exposes the rule `import-oci` selects a platform with** (bead `ley-line-open-370b41`), so a consumer such as Cloister can pick the lock row this host boots without mirroring the rule.
  - `Platform: FromStr` parses `os/architecture[/variant]` and refuses anything else by name.
  - `Platform::admits(&Platform)` decides whether this platform runs an image for the candidate platform:
    - `os` and `architecture` must match, by OCI's names (`amd64`, never `x86_64`);
    - a platform without a variant admits any variant;
    - `arm64` without a variant is `arm64/v8`;
    - an empty variant is no variant.
  - `Platform::select(&[candidates])` returns the index of the one platform string this platform runs. It refuses none or several as the new `PlatformError::{NoMatch, Ambiguous}`, naming the candidates, and refuses a malformed string as `PlatformError::Malformed`.
  - `PlatformError` is `#[non_exhaustive]`. `Ambiguous` advises naming a variant only to a platform that names none.
  - An empty `variant` is now no variant everywhere: `with_variant("")`, `Display`, and the import's selection and config check.
  - As a result, an index with an `arm64` entry whose variant is `""` beside an `arm64/v8` entry is now refused as `AmbiguousPlatform` under a `--platform`-style `arm64/v8` request, as `arm64` beside `arm64/v8` already was. A config whose variant is `""` is now admitted for `arm64/v8`. `import-oci` requests no variant, so its outcomes are unchanged.
