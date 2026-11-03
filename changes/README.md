# Pending CHANGELOG entries

Each change adds one file here instead of editing `CHANGELOG.md`
(bead `ley-line-open-465ce2`). Two PRs that each add a file touch different
paths, so they merge in either order without a conflict.

- **Name:** `<bead-id>.md` or `<bead-id>-<slug>.md`, for example
  `ley-line-open-465ce2.md` or `ley-line-open-465ce2-fragments.md`.
- **First line:** `Section: <name>`, one of `Security`, `Breaking`, `Added`,
  `Changed`, `Fixed`, `Removed`.
- **Rest of the file:** the entry, in the style of the existing CHANGELOG:

```markdown
Section: Fixed

- **What changed, in one bold clause** (bead `ley-line-open-xxxxxx`).
  Why it matters and what a user sees.
```

`task lint:changelog-fragments` (run by `task ci`) checks every fragment.
At release, `tools/bump_release_version.sh` writes the fragments into the new
version's section in the order above, Security first, and deletes them.
