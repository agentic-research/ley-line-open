//! `leyline self` — the binary manages its own installation
//! (bead `ley-line-open-321ded`).
//!
//! Motivating failure: mache's `task install` drops the leyline binary
//! into `~/.mache`, which is on nobody's PATH — an "installed" leyline
//! no shell can find. The fix is the rustup/uv/deno model: the binary
//! owns install/update as a first-class product surface instead of
//! every consumer reinventing a copy step in a Taskfile.
//!
//! Three commands:
//!   * `leyline self install [--bin-dir DIR]` — copy the CURRENT
//!     executable into a stable directory. Default `~/.local/bin`;
//!     `$LEYLINE_INSTALL_DIR` overrides the default; `--bin-dir` wins
//!     over both. Idempotent (same bytes → reported no-op) and atomic
//!     (same-dir staging file + rename, so a crash leaves either the
//!     old binary or the new one — never a torn file).
//!   * `leyline self update [--version vX.Y.Z] [--force]` — resolve a
//!     release on github.com/agentic-research/ley-line-open (latest,
//!     or the pinned version), download the platform asset and the
//!     release's SHA256SUMS, verify the digest BEFORE anything touches
//!     the installed binary, then atomically swap it in. The previous
//!     binary is kept alongside as `leyline.prev` for rollback.
//!     Downgrades are refused without `--force`.
//!   * `leyline self where` — print the resolved install dir, whether
//!     it is on PATH, and the running binary's path.
//!
//! PATH is never mutated: when the install dir is off PATH we print
//! the exact one-line export for the user's shell and stop. Editing
//! rc files is a line this surface does not cross.
//!
//! Testability: every decision is a function over an injected
//! [`SelfContext`] — process env, current exe, and the release base
//! URLs all arrive as data (`lib.rs` builds the real one at the
//! dispatch boundary). Tests drive the full update flow against
//! `file://` fixture trees: no network, no listeners, no sleeps.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// The one binary name this surface manages. Asset names, install
/// destinations, and the `.prev` rollback file all derive from it.
pub const BIN_NAME: &str = "leyline";

/// Release-JSON endpoint root (GitHub REST). `{api_base}/releases/latest`
/// resolves the latest tag; public releases need no token, but
/// `$GITHUB_TOKEN` is honored for rate limits when present.
const RELEASE_API_BASE: &str = "https://api.github.com/repos/agentic-research/ley-line-open";

/// Asset download root. `{download_base}/{tag}/{asset}` is GitHub's
/// stable per-release download URL shape.
const RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/agentic-research/ley-line-open/releases/download";

/// Digest manifest published alongside every release's binaries.
const SUMS_ASSET: &str = "SHA256SUMS";

/// Sent on every HTTP request — the GitHub API rejects UA-less callers.
const USER_AGENT: &str = concat!("leyline/", env!("CARGO_PKG_VERSION"));

/// Download ceiling for a single release file. A literal (not an
/// expression) so there is no arithmetic to silently drift; release
/// binaries are O(100 MB), this is comfortably above any legitimate
/// asset while still bounding a malicious endless body.
const MAX_DOWNLOAD_BYTES: u64 = 2_147_483_648;

/// Everything `leyline self` reads from the process, captured as data.
/// `from_process()` is called exactly once, at the `lib.rs` dispatch
/// boundary; every function below takes the context by reference so
/// tests can substitute temp dirs and `file://` fixture roots.
#[derive(Debug, Clone)]
pub struct SelfContext {
    /// The user's home dir (`dirs::home_dir()`); anchors the default
    /// install dir `~/.local/bin`.
    pub home: Option<PathBuf>,
    /// `$LEYLINE_INSTALL_DIR`, if set and non-empty.
    pub env_install_dir: Option<String>,
    /// `$PATH`, verbatim (empty string when unset).
    pub path_env: String,
    /// `$SHELL`, used only to phrase the PATH hint — never to edit rc files.
    pub shell: Option<String>,
    /// `std::env::current_exe()`; `None` when the OS cannot say.
    pub current_exe: Option<PathBuf>,
    /// The running binary's version (daemon::version::BINARY_VERSION —
    /// the crate's one version authority). Downgrade refusal compares
    /// release tags against this.
    pub current_version: String,
    /// Release-JSON root; `file://` fixture roots in tests.
    pub api_base: String,
    /// Asset download root; `file://` fixture roots in tests.
    pub download_base: String,
    /// `$GITHUB_TOKEN`, if set and non-empty. Optional — public releases
    /// need no auth; this only buys API rate-limit headroom.
    pub github_token: Option<String>,
}

impl SelfContext {
    /// Capture the real process environment. The only place in this
    /// module allowed to touch `std::env`.
    pub fn from_process() -> Self {
        Self {
            home: dirs::home_dir(),
            env_install_dir: env_non_empty(std::env::var("LEYLINE_INSTALL_DIR")),
            path_env: std::env::var("PATH").unwrap_or_default(),
            shell: env_non_empty(std::env::var("SHELL")),
            current_exe: std::env::current_exe().ok(),
            current_version: crate::daemon::version::BINARY_VERSION.to_string(),
            api_base: RELEASE_API_BASE.to_string(),
            download_base: RELEASE_DOWNLOAD_BASE.to_string(),
            github_token: env_non_empty(std::env::var("GITHUB_TOKEN")),
        }
    }
}

/// An env var is meaningful only when set AND non-empty — an exported
/// `LEYLINE_INSTALL_DIR=` must not select "" as an install dir. A
/// module-level fn (not a closure inside `from_process`) so its
/// falsifier can hit it without mutating process env from a test.
fn env_non_empty(value: std::result::Result<String, std::env::VarError>) -> Option<String> {
    value.ok().filter(|s| !s.is_empty())
}

// ─────────────────────────────────────────────────────────────────────
// Install-dir resolution + PATH detection
// ─────────────────────────────────────────────────────────────────────

/// Resolve the install dir: `--bin-dir` flag > `$LEYLINE_INSTALL_DIR`
/// > `~/.local/bin`. No `~` expansion — callers pass real paths.
fn resolve_install_dir(ctx: &SelfContext, flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = flag {
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = &ctx.env_install_dir {
        return Ok(PathBuf::from(dir));
    }
    let home = ctx
        .home
        .as_ref()
        .context("cannot resolve an install dir: no --bin-dir, no $LEYLINE_INSTALL_DIR, and no home directory")?;
    Ok(home.join(".local").join("bin"))
}

/// Is `dir` one of `path_env`'s entries? Compares physical paths
/// (canonicalized) when both sides resolve, falling back to the literal
/// comparison for entries that don't exist — the same discipline as the
/// CI feature-reachability gate, so a symlinked `~/.local/bin` still
/// counts as on PATH.
fn dir_on_path(dir: &Path, path_env: &str) -> bool {
    let want_physical = dir.canonicalize().ok();
    std::env::split_paths(path_env).any(|entry| {
        if entry.as_os_str().is_empty() {
            return false;
        }
        if entry == dir {
            return true;
        }
        match (&want_physical, entry.canonicalize().ok()) {
            (Some(want), Some(got)) => *want == got,
            _ => false,
        }
    })
}

/// The exact one-line command that puts `dir` on PATH in the user's
/// shell. Printed, never executed — rc files belong to the user.
/// `pub` because `leyline doctor`'s warn-only PATH check phrases its
/// hint with the same line.
pub fn export_hint(dir: &Path, shell: Option<&str>) -> String {
    let is_fish = shell
        .map(Path::new)
        .and_then(Path::file_name)
        .is_some_and(|name| name == "fish");
    if is_fish {
        format!("fish_add_path {}", dir.display())
    } else {
        // POSIX-family (bash/zsh/sh) all accept the same export line.
        format!("export PATH=\"{}:$PATH\"", dir.display())
    }
}

/// The running binary's path and whether its directory is on PATH.
/// `None` when the OS cannot report the current exe. `leyline doctor`
/// consumes this for its warn-only reachability check.
pub fn running_binary_status(ctx: &SelfContext) -> Option<(PathBuf, bool)> {
    let exe = ctx.current_exe.clone()?;
    let dir = exe.parent()?;
    let on_path = dir_on_path(dir, &ctx.path_env);
    Some((exe, on_path))
}

// ─────────────────────────────────────────────────────────────────────
// Atomic write + install
// ─────────────────────────────────────────────────────────────────────

/// Staging file for an atomic write, ALWAYS in the destination's own
/// directory: `rename(2)` is only atomic within one filesystem, so
/// staging anywhere else (e.g. /tmp) would reintroduce torn-file risk.
/// Pid-suffixed so concurrent installs from different processes don't
/// clobber each other's staging bytes.
fn staging_path(dir: &Path) -> PathBuf {
    dir.join(format!(".{BIN_NAME}.staging.{}", std::process::id()))
}

/// Write `bytes` as an executable at `dest`: staging file in the same
/// dir, mode 0755, then a single rename. Any failure removes the
/// staging file — a failed install never leaves residue, and `dest`
/// still holds whatever it held before.
fn write_executable_atomic(bytes: &[u8], dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .context("install destination has no parent directory")?;
    let staging = staging_path(dir);
    let attempt = (|| -> Result<()> {
        fs::write(&staging, bytes)
            .with_context(|| format!("failed to write staging file {}", staging.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
        }
        fs::rename(&staging, dest)
            .with_context(|| format!("failed to move staging file into {}", dest.display()))?;
        Ok(())
    })();
    if attempt.is_err() {
        let _ = fs::remove_file(&staging);
    }
    attempt
}

/// What `self install` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The binary was (re)written.
    Installed,
    /// Destination already holds byte-identical content — no-op.
    AlreadyCurrent,
}

/// Install `bytes` as `{dir}/leyline`, creating `dir` if needed.
/// Idempotent: byte-identical content short-circuits to
/// [`InstallOutcome::AlreadyCurrent`] without writing anything.
fn install_bytes(bytes: &[u8], dir: &Path) -> Result<InstallOutcome> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create install dir {}", dir.display()))?;
    let dest = dir.join(BIN_NAME);
    if dest.exists() && fs::read(&dest)? == bytes {
        return Ok(InstallOutcome::AlreadyCurrent);
    }
    write_executable_atomic(bytes, &dest)?;
    Ok(InstallOutcome::Installed)
}

/// `self install`'s result, kept structured so tests assert on facts
/// rather than stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub outcome: InstallOutcome,
    pub dest: PathBuf,
    pub dir_on_path: bool,
    /// The one-line PATH fix, present exactly when the dir is off PATH.
    pub path_hint: Option<String>,
}

/// `leyline self install [--bin-dir DIR]`.
pub fn self_install(ctx: &SelfContext, bin_dir: Option<&Path>) -> Result<InstallReport> {
    let dir = resolve_install_dir(ctx, bin_dir)?;
    let exe = ctx
        .current_exe
        .as_ref()
        .context("cannot determine the running executable's path")?;
    let bytes = fs::read(exe)
        .with_context(|| format!("failed to read running binary {}", exe.display()))?;
    let outcome = install_bytes(&bytes, &dir)?;
    let dest = dir.join(BIN_NAME);
    let on_path = dir_on_path(&dir, &ctx.path_env);
    let path_hint = (!on_path).then(|| export_hint(&dir, ctx.shell.as_deref()));

    match outcome {
        InstallOutcome::Installed => {
            println!("installed {} -> {}", exe.display(), dest.display());
        }
        InstallOutcome::AlreadyCurrent => {
            println!(
                "{} already matches the running binary — nothing to do",
                dest.display()
            );
        }
    }
    if let Some(hint) = &path_hint {
        // Warn + hint only. We never edit rc files.
        println!("warning: {} is not on PATH", dir.display());
        println!("  add it with: {hint}");
    }
    Ok(InstallReport {
        outcome,
        dest,
        dir_on_path: on_path,
        path_hint,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Release resolution + digest verification
// ─────────────────────────────────────────────────────────────────────

/// The release asset for this build's platform. Names MUST match
/// `tools/release-assets.txt` (`leyline-{darwin|linux}-{amd64|arm64}`,
/// Go-style arch spelling) — the release pipeline owns that file, this
/// function conforms to it.
fn platform_asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("no published leyline release asset for OS '{other}'"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => bail!("no published leyline release asset for architecture '{other}'"),
    };
    Ok(format!("{BIN_NAME}-{os}-{arch}"))
}

/// Fetch one URL fully into memory. `file://` bypasses HTTP entirely
/// (the fixture path tests ride on), everything else goes through ureq
/// with rustls. Non-2xx statuses surface as errors from `call()`.
fn fetch(url: &str, token: Option<&str>) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).with_context(|| format!("failed to read {url}"));
    }
    let mut req = ureq::get(url).header("User-Agent", USER_AGENT);
    if let Some(token) = token {
        // Rate-limit headroom only; public releases need no auth. ureq
        // strips Authorization on cross-host redirects (GitHub asset
        // downloads bounce to a storage host), which is the behavior
        // GitHub documents as required.
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    let mut resp = req.call().with_context(|| format!("GET {url} failed"))?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
        .with_context(|| format!("failed reading response body of {url}"))?;
    Ok(bytes)
}

/// Resolve the latest release tag via `{api_base}/releases/latest`.
fn latest_tag(ctx: &SelfContext) -> Result<String> {
    let url = format!("{}/releases/latest", ctx.api_base);
    let bytes = fetch(&url, ctx.github_token.as_deref())?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("{url} is not release JSON"))?;
    json.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .with_context(|| format!("release JSON from {url} has no tag_name"))
}

/// Parse `vX.Y.Z` / `X.Y.Z` into a comparable triple. Anything else is
/// an error — this surface only ships plain semver tags.
fn parse_version(version: &str) -> Result<(u64, u64, u64)> {
    let trimmed = version.trim();
    let bare = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let parts: Vec<&str> = bare.split('.').collect();
    if parts.len() != 3 {
        bail!("unrecognized version '{version}' (expected X.Y.Z or vX.Y.Z)");
    }
    let mut nums = [0u64; 3];
    for (slot, part) in nums.iter_mut().zip(&parts) {
        *slot = part.parse().with_context(|| {
            format!("unrecognized version '{version}' (expected X.Y.Z or vX.Y.Z)")
        })?;
    }
    Ok((nums[0], nums[1], nums[2]))
}

/// Whether an update to `target` should proceed from `current`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateDecision {
    /// Same version and no `--force` — nothing to download.
    AlreadyCurrent,
    Proceed,
}

/// Downgrade policy in one place: newer proceeds, equal is a no-op,
/// older is refused — and `--force` overrides all of it (including
/// same-version reinstall, rustup-style).
fn decide_update(current: &str, target: &str, force: bool) -> Result<UpdateDecision> {
    let cur = parse_version(current)?;
    let tgt = parse_version(target)?;
    if force {
        return Ok(UpdateDecision::Proceed);
    }
    match tgt.cmp(&cur) {
        Ordering::Equal => Ok(UpdateDecision::AlreadyCurrent),
        Ordering::Less => {
            bail!("refusing to downgrade from {current} to {target}; pass --force to override")
        }
        Ordering::Greater => Ok(UpdateDecision::Proceed),
    }
}

/// Find `asset`'s digest in a coreutils-format SHA256SUMS body
/// (`<hex>  <name>`, optional `*` binary-mode marker on the name).
fn expected_digest(sums: &str, asset: &str) -> Result<String> {
    for line in sums.lines() {
        let mut fields = line.split_whitespace();
        let (Some(digest), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == asset {
            return Ok(digest.to_ascii_lowercase());
        }
    }
    bail!("{SUMS_ASSET} has no entry for '{asset}'")
}

/// The gate every downloaded byte must pass before it may touch the
/// installed binary. Fails closed: mismatch → error, disk untouched.
fn verify_digest(bytes: &[u8], sums: &str, asset: &str) -> Result<()> {
    let expected = expected_digest(sums, asset)?;
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected {
        bail!(
            "digest mismatch for '{asset}': {SUMS_ASSET} says {expected}, \
             downloaded bytes hash to {actual} — refusing to install"
        );
    }
    Ok(())
}

/// `self update`'s result, structured for the same reason as
/// [`InstallReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateReport {
    /// Already at the requested version — nothing downloaded.
    AlreadyCurrent { version: String },
    Updated {
        from: String,
        to: String,
        dest: PathBuf,
        /// Rollback copy of the replaced binary (`leyline.prev`).
        prev: PathBuf,
    },
}

/// `leyline self update [--version vX.Y.Z] [--force]`.
///
/// Ordering is the contract: resolve tag → downgrade policy → download
/// asset + SHA256SUMS into memory → verify digest → only then copy the
/// installed binary to `.prev` and atomically swap the new one in. An
/// error anywhere before the swap leaves the installed binary
/// byte-identical to what it was.
pub fn self_update(
    ctx: &SelfContext,
    requested: Option<&str>,
    force: bool,
) -> Result<UpdateReport> {
    let tag = match requested {
        Some(v) => v.to_string(),
        None => latest_tag(ctx)?,
    };
    if decide_update(&ctx.current_version, &tag, force)? == UpdateDecision::AlreadyCurrent {
        println!(
            "leyline {} is already the requested version — nothing to do",
            ctx.current_version
        );
        return Ok(UpdateReport::AlreadyCurrent {
            version: ctx.current_version.clone(),
        });
    }

    let dir = resolve_install_dir(ctx, None)?;
    let installed = dir.join(BIN_NAME);
    if !installed.exists() {
        bail!(
            "no installed binary at {} — run `leyline self install` first",
            installed.display()
        );
    }

    let asset = platform_asset_name()?;
    let token = ctx.github_token.as_deref();
    let asset_bytes = fetch(&format!("{}/{tag}/{asset}", ctx.download_base), token)?;
    let sums_bytes = fetch(&format!("{}/{tag}/{SUMS_ASSET}", ctx.download_base), token)?;
    let sums = std::str::from_utf8(&sums_bytes).context("SHA256SUMS is not UTF-8")?;
    verify_digest(&asset_bytes, sums, &asset)?;

    // Verified. Keep the old binary as a rollback COPY (not a rename:
    // between a rename-away and a rename-in there would be a window
    // with no `leyline` at all), then swap the new bytes in atomically.
    let prev = installed.with_extension("prev");
    fs::copy(&installed, &prev)
        .with_context(|| format!("failed to keep rollback copy {}", prev.display()))?;
    write_executable_atomic(&asset_bytes, &installed)?;

    println!(
        "updated {} : {} -> {} (previous kept at {})",
        installed.display(),
        ctx.current_version,
        tag,
        prev.display()
    );
    Ok(UpdateReport::Updated {
        from: ctx.current_version.clone(),
        to: tag,
        dest: installed,
        prev,
    })
}

// ─────────────────────────────────────────────────────────────────────
// where
// ─────────────────────────────────────────────────────────────────────

/// `leyline self where`. Returns the printed report so its shape is a
/// tested, stable contract (install scripts grep this).
pub fn self_where(ctx: &SelfContext) -> Result<String> {
    let dir = resolve_install_dir(ctx, None)?;
    let on_path = dir_on_path(&dir, &ctx.path_env);
    let path_note = if on_path { "on PATH" } else { "NOT on PATH" };
    let running = ctx
        .current_exe
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let report = format!(
        "install dir: {} ({path_note})\nrunning binary: {running}\n",
        dir.display()
    );
    print!("{report}");
    Ok(report)
}

// ─────────────────────────────────────────────────────────────────────
// In-module falsifiers (the diff-scoped mutants gate sees lib tests
// only, so every contract above gets its killer here).
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A context rooted in temp fixtures — no test below reads process
    /// env or the network.
    fn fixture_ctx() -> SelfContext {
        SelfContext {
            home: None,
            env_install_dir: None,
            path_env: String::new(),
            shell: None,
            current_exe: None,
            current_version: "0.1.0".to_string(),
            api_base: "file:///nonexistent/api".to_string(),
            download_base: "file:///nonexistent/download".to_string(),
            github_token: None,
        }
    }

    /// Write a fixture release: `{root}/{tag}/{asset}` + SHA256SUMS
    /// whose digest line hashes `sums_of` (pass different bytes than
    /// `asset_bytes` to build a corrupted release).
    fn write_release(root: &Path, tag: &str, asset_bytes: &[u8], sums_of: &[u8]) {
        let asset = platform_asset_name().unwrap();
        let dir = root.join(tag);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(&asset), asset_bytes).unwrap();
        let digest = hex::encode(Sha256::digest(sums_of));
        fs::write(dir.join(SUMS_ASSET), format!("{digest}  {asset}\n")).unwrap();
    }

    #[test]
    fn install_dir_resolution_is_flag_then_env_then_home_default() {
        let mut ctx = fixture_ctx();
        ctx.home = Some(PathBuf::from("/home/u"));
        ctx.env_install_dir = Some("/env/bin".to_string());
        // Flag wins over everything.
        assert_eq!(
            resolve_install_dir(&ctx, Some(Path::new("/flag/bin"))).unwrap(),
            PathBuf::from("/flag/bin")
        );
        // Env wins over the default.
        assert_eq!(
            resolve_install_dir(&ctx, None).unwrap(),
            PathBuf::from("/env/bin")
        );
        // Default anchors at ~/.local/bin.
        ctx.env_install_dir = None;
        assert_eq!(
            resolve_install_dir(&ctx, None).unwrap(),
            PathBuf::from("/home/u/.local/bin")
        );
        // No flag, no env, no home: a hard error, not a guess.
        ctx.home = None;
        assert!(resolve_install_dir(&ctx, None).is_err());
    }

    #[test]
    fn install_is_idempotent_second_run_reports_already_current() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            install_bytes(b"binary-v1", dir.path()).unwrap(),
            InstallOutcome::Installed
        );
        // Same bytes: reported no-op.
        assert_eq!(
            install_bytes(b"binary-v1", dir.path()).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
        // Different bytes: a real reinstall.
        assert_eq!(
            install_bytes(b"binary-v2", dir.path()).unwrap(),
            InstallOutcome::Installed
        );
        assert_eq!(fs::read(dir.path().join(BIN_NAME)).unwrap(), b"binary-v2");
    }

    #[test]
    fn install_stages_in_target_dir_and_leaves_no_residue() {
        let dir = TempDir::new().unwrap();
        // Same-dir staging is the precondition for atomic rename(2) —
        // a staging path on another filesystem would tear.
        assert_eq!(staging_path(dir.path()).parent().unwrap(), dir.path());

        install_bytes(b"payload", dir.path()).unwrap();
        let entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![BIN_NAME.to_string()], "no staging residue");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(BIN_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "installed binary must be executable");
        }
    }

    #[test]
    fn failed_install_cleans_its_staging_file() {
        let dir = TempDir::new().unwrap();
        // Occupy the destination with a DIRECTORY so the final rename
        // fails after staging succeeded.
        fs::create_dir(dir.path().join(BIN_NAME)).unwrap();
        assert!(install_bytes(b"payload", dir.path()).is_err());
        assert!(
            !staging_path(dir.path()).exists(),
            "failed install must remove its staging file"
        );
    }

    #[test]
    fn dir_on_path_detects_presence_absence_and_symlinks() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let joined = std::env::join_paths([a.path(), b.path()])
            .unwrap()
            .into_string()
            .unwrap();
        assert!(dir_on_path(a.path(), &joined));
        assert!(dir_on_path(b.path(), &joined));
        let c = TempDir::new().unwrap();
        assert!(!dir_on_path(c.path(), &joined));
        assert!(!dir_on_path(a.path(), ""));

        // Physical-path comparison: a symlinked PATH entry still counts.
        #[cfg(unix)]
        {
            let link = c.path().join("alias");
            std::os::unix::fs::symlink(a.path(), &link).unwrap();
            let via_link = std::env::join_paths([link.as_path()])
                .unwrap()
                .into_string()
                .unwrap();
            assert!(dir_on_path(a.path(), &via_link));
        }
    }

    #[test]
    fn export_hint_matches_the_users_shell() {
        let dir = Path::new("/x/bin");
        assert_eq!(
            export_hint(dir, Some("/bin/zsh")),
            "export PATH=\"/x/bin:$PATH\""
        );
        assert_eq!(
            export_hint(dir, Some("/opt/homebrew/bin/fish")),
            "fish_add_path /x/bin"
        );
        // Unknown shell falls back to the POSIX line.
        assert_eq!(export_hint(dir, None), "export PATH=\"/x/bin:$PATH\"");
    }

    #[test]
    fn platform_asset_name_matches_published_release_assets() {
        // The published names live in tools/release-assets.txt; this
        // pins conformance for whichever platform runs the suite.
        let name = platform_asset_name().unwrap();
        let published = [
            "leyline-darwin-amd64",
            "leyline-darwin-arm64",
            "leyline-linux-amd64",
            "leyline-linux-arm64",
        ];
        assert!(
            published.contains(&name.as_str()),
            "'{name}' is not a published release asset name"
        );
    }

    #[test]
    fn version_parsing_accepts_v_prefix_and_rejects_junk() {
        assert_eq!(parse_version("v1.2.3").unwrap(), (1, 2, 3));
        assert_eq!(parse_version("0.13.0").unwrap(), (0, 13, 0));
        assert!(parse_version("1.2").is_err());
        assert!(parse_version("release-1.2.3").is_err());
        assert!(parse_version("").is_err());
    }

    #[test]
    fn downgrade_is_refused_without_force() {
        // Older target: refused…
        assert!(decide_update("0.13.0", "v0.12.0", false).is_err());
        // …unless forced.
        assert_eq!(
            decide_update("0.13.0", "v0.12.0", true).unwrap(),
            UpdateDecision::Proceed
        );
        // Equal is a no-op; forced equal is a reinstall.
        assert_eq!(
            decide_update("0.13.0", "v0.13.0", false).unwrap(),
            UpdateDecision::AlreadyCurrent
        );
        assert_eq!(
            decide_update("0.13.0", "v0.13.0", true).unwrap(),
            UpdateDecision::Proceed
        );
        // Newer proceeds.
        assert_eq!(
            decide_update("0.13.0", "v0.14.0", false).unwrap(),
            UpdateDecision::Proceed
        );
        // A garbage tag is an error, not a silent proceed.
        assert!(decide_update("0.13.0", "nightly", false).is_err());
    }

    #[test]
    fn sha256sums_lookup_parses_coreutils_format() {
        let sums = "abc123  leyline-linux-amd64\nDEF456 *leyline-darwin-arm64\n\n";
        assert_eq!(
            expected_digest(sums, "leyline-linux-amd64").unwrap(),
            "abc123"
        );
        // Binary-mode marker stripped; digest normalized to lowercase.
        assert_eq!(
            expected_digest(sums, "leyline-darwin-arm64").unwrap(),
            "def456"
        );
        assert!(expected_digest(sums, "leyline-windows-amd64").is_err());
    }

    /// THE test: a corrupted asset is refused and the installed binary
    /// is byte-for-byte untouched — no swap, no .prev, no staging.
    #[test]
    fn corrupted_asset_is_refused_and_installed_binary_is_untouched() {
        let install = TempDir::new().unwrap();
        let releases = TempDir::new().unwrap();
        let installed = install.path().join(BIN_NAME);
        fs::write(&installed, b"old-binary").unwrap();
        // SHA256SUMS attests to bytes the asset does NOT contain.
        write_release(releases.path(), "v0.2.0", b"evil-binary", b"good-binary");

        let mut ctx = fixture_ctx();
        ctx.env_install_dir = Some(install.path().display().to_string());
        ctx.download_base = format!("file://{}", releases.path().display());

        let err = self_update(&ctx, Some("v0.2.0"), false).unwrap_err();
        assert!(
            err.to_string().contains("digest mismatch"),
            "refusal must name the digest mismatch, got: {err}"
        );
        assert_eq!(
            fs::read(&installed).unwrap(),
            b"old-binary",
            "a refused update must leave the installed binary untouched"
        );
        assert!(!installed.with_extension("prev").exists());
        assert!(!staging_path(install.path()).exists());
    }

    #[test]
    fn verified_update_swaps_binary_and_keeps_prev_for_rollback() {
        let install = TempDir::new().unwrap();
        let releases = TempDir::new().unwrap();
        let installed = install.path().join(BIN_NAME);
        fs::write(&installed, b"old-binary").unwrap();
        write_release(releases.path(), "v0.2.0", b"new-binary", b"new-binary");

        let mut ctx = fixture_ctx();
        ctx.env_install_dir = Some(install.path().display().to_string());
        ctx.download_base = format!("file://{}", releases.path().display());

        let report = self_update(&ctx, Some("v0.2.0"), false).unwrap();
        assert_eq!(
            report,
            UpdateReport::Updated {
                from: "0.1.0".to_string(),
                to: "v0.2.0".to_string(),
                dest: installed.clone(),
                prev: installed.with_extension("prev"),
            }
        );
        assert_eq!(fs::read(&installed).unwrap(), b"new-binary");
        assert_eq!(
            fs::read(installed.with_extension("prev")).unwrap(),
            b"old-binary",
            "the replaced binary must survive as leyline.prev"
        );
    }

    #[test]
    fn update_without_an_installed_binary_points_at_self_install() {
        let install = TempDir::new().unwrap();
        let mut ctx = fixture_ctx();
        ctx.env_install_dir = Some(install.path().display().to_string());
        let err = self_update(&ctx, Some("v0.2.0"), false).unwrap_err();
        assert!(err.to_string().contains("leyline self install"));
    }

    #[test]
    fn same_version_update_is_a_noop_that_never_fetches() {
        // download_base points nowhere: a fetch attempt would error, so
        // Ok proves the no-op path never touches the release source.
        let ctx = fixture_ctx();
        assert_eq!(
            self_update(&ctx, Some("v0.1.0"), false).unwrap(),
            UpdateReport::AlreadyCurrent {
                version: "0.1.0".to_string()
            }
        );
    }

    #[test]
    fn latest_tag_is_resolved_from_release_json() {
        let install = TempDir::new().unwrap();
        let api = TempDir::new().unwrap();
        let releases = TempDir::new().unwrap();
        let installed = install.path().join(BIN_NAME);
        fs::write(&installed, b"old-binary").unwrap();
        fs::create_dir_all(api.path().join("releases")).unwrap();
        fs::write(
            api.path().join("releases/latest"),
            r#"{"tag_name": "v9.9.9"}"#,
        )
        .unwrap();
        write_release(releases.path(), "v9.9.9", b"new-binary", b"new-binary");

        let mut ctx = fixture_ctx();
        ctx.env_install_dir = Some(install.path().display().to_string());
        ctx.api_base = format!("file://{}", api.path().display());
        ctx.download_base = format!("file://{}", releases.path().display());

        match self_update(&ctx, None, false).unwrap() {
            UpdateReport::Updated { to, .. } => assert_eq!(to, "v9.9.9"),
            other => panic!("expected Updated, got {other:?}"),
        }
        assert_eq!(fs::read(&installed).unwrap(), b"new-binary");
    }

    #[test]
    fn self_install_installs_the_running_binary_and_reports_path_status() {
        let fake_exe_dir = TempDir::new().unwrap();
        let install = TempDir::new().unwrap();
        let exe = fake_exe_dir.path().join("leyline-under-test");
        fs::write(&exe, b"running-binary").unwrap();

        let mut ctx = fixture_ctx();
        ctx.current_exe = Some(exe);
        let target = install.path().join("bin");

        // Off PATH: report carries the hint.
        let report = self_install(&ctx, Some(&target)).unwrap();
        assert_eq!(report.outcome, InstallOutcome::Installed);
        assert_eq!(fs::read(&report.dest).unwrap(), b"running-binary");
        assert!(!report.dir_on_path);
        assert_eq!(
            report.path_hint.as_deref(),
            Some(format!("export PATH=\"{}:$PATH\"", target.display()).as_str())
        );

        // Second run: no-op; and with the dir on PATH the hint is gone.
        ctx.path_env = std::env::join_paths([target.as_path()])
            .unwrap()
            .into_string()
            .unwrap();
        let report = self_install(&ctx, Some(&target)).unwrap();
        assert_eq!(report.outcome, InstallOutcome::AlreadyCurrent);
        assert!(report.dir_on_path);
        assert_eq!(report.path_hint, None);
    }

    #[test]
    fn where_report_shape_is_stable() {
        // Install scripts grep this output — its shape is a contract.
        let mut ctx = fixture_ctx();
        ctx.env_install_dir = Some("/x/bin".to_string());
        ctx.current_exe = Some(PathBuf::from("/y/leyline"));
        assert_eq!(
            self_where(&ctx).unwrap(),
            "install dir: /x/bin (NOT on PATH)\nrunning binary: /y/leyline\n"
        );
        ctx.path_env = "/x/bin".to_string();
        assert_eq!(
            self_where(&ctx).unwrap(),
            "install dir: /x/bin (on PATH)\nrunning binary: /y/leyline\n"
        );
    }

    #[test]
    fn running_binary_status_reports_exe_and_path_membership() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join(BIN_NAME);
        fs::write(&exe, b"x").unwrap();

        let mut ctx = fixture_ctx();
        assert_eq!(running_binary_status(&ctx), None, "no exe → no status");

        ctx.current_exe = Some(exe.clone());
        assert_eq!(running_binary_status(&ctx), Some((exe.clone(), false)));

        ctx.path_env = std::env::join_paths([dir.path()])
            .unwrap()
            .into_string()
            .unwrap();
        assert_eq!(running_binary_status(&ctx), Some((exe, true)));
    }

    #[test]
    fn env_values_count_only_when_set_and_non_empty() {
        // `export LEYLINE_INSTALL_DIR=` (empty) must not select "" as
        // an install dir — empty and unset are the same non-signal.
        assert_eq!(env_non_empty(Ok("x".to_string())), Some("x".to_string()));
        assert_eq!(env_non_empty(Ok(String::new())), None);
        assert_eq!(env_non_empty(Err(std::env::VarError::NotPresent)), None);
    }

    #[test]
    fn from_process_captures_the_real_process_identity() {
        let ctx = SelfContext::from_process();
        assert_eq!(ctx.current_version, crate::daemon::version::BINARY_VERSION);
        assert_eq!(
            ctx.api_base,
            "https://api.github.com/repos/agentic-research/ley-line-open"
        );
        assert_eq!(
            ctx.download_base,
            "https://github.com/agentic-research/ley-line-open/releases/download"
        );
        assert_eq!(ctx.current_exe, std::env::current_exe().ok());
        assert_eq!(ctx.path_env, std::env::var("PATH").unwrap_or_default());
    }
}
