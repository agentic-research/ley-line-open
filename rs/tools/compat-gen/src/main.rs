//! `compat-gen` — emit `compatibility.json` from the daemon's runtime
//! version constants. Self-maintaining compat surface per bead
//! `ley-line-open-cbea02`.
//!
//! This binary reads `leyline_cli_lib::daemon::version::*` (the same
//! constants the `leyline_version` op surfaces over the wire) and
//! prints a `compatibility.json` document to stdout. The CI invariant
//! (Taskfile.yml `compat:check`) is: regenerate, diff against the
//! committed `compatibility.json` at the repo root, fail the build
//! if they differ. Same discipline `clients/go/leyline-schema/regen.sh`
//! uses for Go bindings.
//!
//! There is no hand-maintained version matrix. Adding a new
//! compatible-client floor or bumping the wire-format major means
//! editing `rs/ll-open/cli-lib/src/daemon/version.rs` exactly once;
//! everything downstream — the live `leyline_version` op response, the
//! committed `compatibility.json`, the eventual GitHub release asset
//! — derives from that single edit.
//!
//! # Output schema
//!
//! ```json
//! {
//!   "$schema_version": 1,
//!   "binary_version": "0.4.5",
//!   "schema_version": "0.4.5",
//!   "wire_format_major": 1,
//!   "compat_min_schema_version": "0.4.1",
//!   "build_date": "unspecified"
//! }
//! ```
//!
//! `$schema_version` is the schema version of this document itself
//! (versus the daemon's `wire_format_major`). Consumers that parse
//! `compatibility.json` should gate on it before reading the rest of
//! the fields.

use leyline_cli_lib::daemon::version;
use serde::Serialize;

/// Schema version of the `compatibility.json` document itself. Bumps
/// when this binary's output shape changes (adds fields, removes
/// fields, renames). Independent of the daemon's `wire_format_major`.
const COMPAT_DOC_SCHEMA_VERSION: u32 = 1;

/// One row of the compat surface. Field order matches what serde_json
/// emits in pretty-print mode — stable across regens so the committed
/// file diffs cleanly when constants change.
#[derive(Serialize)]
struct CompatibilityDoc {
    /// Schema version of this document. See `COMPAT_DOC_SCHEMA_VERSION`.
    #[serde(rename = "$schema_version")]
    schema_version_doc: u32,

    /// Daemon binary version. Equals `version::BINARY_VERSION` (which
    /// equals `CARGO_PKG_VERSION` of `leyline-cli-lib` at build time).
    binary_version: &'static str,

    /// Schema-client version this daemon targets. Today equals
    /// `binary_version`; see `version::SCHEMA_VERSION`.
    schema_version: &'static str,

    /// Current JSON wire envelope major. Bumps on incompatible
    /// changes. See `version::WIRE_FORMAT_MAJOR`.
    wire_format_major: u32,

    /// Earliest compatible schema-client version. See
    /// `version::COMPAT_MIN_SCHEMA_VERSION`.
    compat_min_schema_version: &'static str,

    /// ISO-8601 build date or `"unspecified"`. See
    /// `version::BUILD_DATE`.
    build_date: &'static str,
}

fn main() -> anyhow::Result<()> {
    let doc = CompatibilityDoc {
        schema_version_doc: COMPAT_DOC_SCHEMA_VERSION,
        binary_version: version::BINARY_VERSION,
        schema_version: version::SCHEMA_VERSION,
        wire_format_major: version::WIRE_FORMAT_MAJOR,
        compat_min_schema_version: version::COMPAT_MIN_SCHEMA_VERSION,
        build_date: version::BUILD_DATE,
    };
    // Pretty-print with a trailing newline — diffs against the
    // committed file should compare line-for-line.
    let mut s = serde_json::to_string_pretty(&doc)?;
    s.push('\n');
    print!("{s}");
    Ok(())
}
