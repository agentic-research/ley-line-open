Section: Changed

- **A build that is not the tagged release reports `X.Y.Z-dev+<shortsha>`,
  so it can no longer pass for the release** (bead `ley-line-open-5139a3`).
  `leyline --version`, the `leyline_version` op's `binary_version`, MCP
  `serverInfo.version`, the `leyline self` user agent and the arena owner
  record's `leyline_version` all carry `daemon::version::BINARY_VERSION`.
  It is the bare crate version only when `LEYLINE_RELEASE_VERSION` names it,
  which `release.yml` sets from the pushed tag for the binary and image
  builds; a value naming another version fails the build. Every other build
  appends `-dev+<12-hex shortsha>`, plus `.dirty` when tracked files differ
  from `HEAD`, or `-dev+unknown` without git. The suffix comes from git and
  that variable, never the clock, so one revision reports one version.
  - `daemon::version::RELEASE_VERSION` is the bare release line;
    `compatibility.json`, `server.json` and `leyline self update`'s
    downgrade check use it, so the committed artifacts do not vary per
    commit.
  - `tools/cli_smoke.sh` requires the dev form when `EXPECT_VERSION` is
    unset (the `ci` and dry-run smokes) and the exact bare version when it
    is set; `release.yml` sets it to the tag.
  - A consumer pinning an exact version (mache's `leyline --version` check)
    now sees a dev build as unequal to the release.
