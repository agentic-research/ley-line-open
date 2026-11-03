Section: Changed

- **CHANGELOG entries are fragments, one file per change** (bead
  `ley-line-open-465ce2`). A change adds `changes/<bead-id>.md` naming its
  section on the first line instead of editing `CHANGELOG.md`, so two PRs no
  longer conflict on the `[Unreleased]` block and force each other to rebase
  and rerun CI. `tools/bump_release_version.sh` assembles the fragments into
  the release's section, Security first, and deletes them;
  `task lint:changelog-fragments`, run by `task ci`, checks their names,
  sections, and text, and `tools/test_changelog_fragments.sh` proves two
  PRs' fragments merge in either order and assemble identically.
  `task release:hypervisor:record` now builds in `rs/target-hypervisor`, so
  it runs beside `task ci` instead of contending for `rs/target`.
