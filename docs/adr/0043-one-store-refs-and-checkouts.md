# ADR-0043 — One store: branches are refs, worktrees are checkouts, git is read natively

**Status:** Proposed (2026-10-14)
**Bead:** `ley-line-open-60f39f` (a daemon restart with `--source` reverts accepted proposals)
**Related:**

- ADR-0028 (content-addressed source blobs), ADR-0029 (CAS-backed workspace — Proposed)
- ADR-0040 (the write boundary; D3 says the write path never touches disk)
- ADR-0041 (publication), ADR-0042 (proposals)
- `ley-line-open-6116c9` (an NFS mount edit's base is taken at its first WRITE)
- `leyline-vcs` (`VersionedGraph`, a `jj-lib` sidecar that versions arena generations)

______________________________________________________________________

## Context

An end-to-end test of v0.20.5 found that accepted proposals do not survive a daemon restart.
`leyline daemon --source <dir>` parses `<dir>` at startup. A proposal stores its bytes in
`source_blobs` and never writes `<dir>` (ADR-0040 D3). On a warm start the incremental reparse sees
that a proposed file's disk bytes differ from its stored hash, counts the file as changed, and
replaces the accepted bytes with the disk bytes. The test lost every proposal this way.

The cause is two authorities for one tree. The daemon treats the directory as authoritative at
startup and in its git watcher; proposals treat `source_blobs` as authoritative. Writing proposals
through to the directory narrows the gap and does not close it. A crash between COMMIT and the
disk write reverts the proposal. A person editing the directory while the daemon is down is
indistinguishable from a proposal the disk never received. Several branches or worktrees of one
repository have no representation at all: each daemon is one directory, one database, one tree.

git solves the same problem with one object store, refs, and per-worktree indexes. The index is
the record of what was last checked out into a directory, and it is what lets git tell "the
directory changed" from "the branch moved". LLO has the store (`source_blobs`, ADR-0028) and the
tree (`_source` and `treeRoot`, ADR-0040). It lacks refs, checkouts and an index.

LLO shells out to `git` today (`git rev-parse HEAD`, `git status --porcelain` in
`cmd_daemon.rs`). It already depends on `jj-lib`, which reads git repositories natively through
`gix`, for `leyline-vcs`.

## Decision

### R1 — One store per repository

One database holds every branch and worktree of a repository. `source_blobs` is shared: a blob is
stored once whichever trees contain it. Content addressing makes branches cost what they differ
by.

### R2 — A ref names a tree

A ref is a row: `name → (treeRoot, parent)`. A branch is a ref. A proposal targets a ref: its bases
are checked against that ref's tree, and on acceptance the ref advances to the new `treeRoot` with
the old one as parent. Proposals to different refs never conflict. Publication (ADR-0041) names
the ref a snapshot shows.

### R3 — A checkout binds a directory to a ref, with an index

A checkout is `(directory, ref, index)`. The index is the tree last materialized into the
directory or last ingested from it, as `path → blob hash`. Writing an accepted proposal to every
checkout of its ref updates their indexes. Reconciling a checkout compares three trees per path:
the directory's bytes, the index, and the ref.

| directory vs index | ref vs index | action |
|---|---|---|
| same | same | nothing |
| same | moved | materialize the ref's bytes into the directory (temp file, rename) |
| changed | same | ingest the directory's bytes as a proposal to the ref, based on the index |
| changed | moved | conflict: reported, nothing overwritten |

Startup and the watcher both run this. A crash between COMMIT and materialization leaves the
directory equal to the index while the ref has moved, so the next reconcile materializes the
proposal instead of reverting it. A person's edit made while the daemon was down shows as the
directory changing against the index and is ingested, not lost. ADR-0040 D3 still holds for the
write path itself: a proposal commits to the store, and materialization is the checkout's job.

### R4 — git is read and written natively

git interop goes through `gix` (directly, or through `jj-lib`, which LLO already depends on), never
through a `git` subprocess. The git blob id is SHA-1 over `"blob <len>\0" ‖ bytes` and is computed
from stored bytes, so `source_blobs` maps to git objects without git. Importing a commit records
its tree as a ref's tree. Exporting a ref writes git blobs, trees and a commit through `gix`. The
`git rev-parse` and `git status` subprocesses in `cmd_daemon.rs` are replaced by reconciliation
(R3) and `gix` reads.

### R5 — The projection follows the refs that are checked out

The projection (ring 1) is rebuildable from a ref's tree. A daemon projects the refs that have
checkouts or are named on its command line. Rows for a blob shared by two refs are derived once:
`node_hash` and CDC are content-addressed already (ADR-0034, ADR-0028). How projection rows name
their ref is the open question below.

## Consequences

- `60f39f` closes by R3, not by write-through alone.
- `--source <dir>` becomes "check out the current branch of `<dir>`'s repository into `<dir>`".
- An agent on a branch is a checkout of that ref: a directory, or a mount over the ref's tree
  (ADR-0029), with no `git worktree add`.
- The NFS base question (`6116c9`) narrows: a mount is a checkout, and its base is the ref's tree
  at the time the client read, which the index records.
- `leyline-vcs` versions arena generations through `jj` today; under R2 it versions refs, and its
  sidecar becomes an export of refs to git.

## Open questions

1. **Projection keying.** Either one projection per checked-out ref (simple, and a duplicate
   per branch), or rows keyed by `(treeRoot, path)` and shared across refs (one copy, and every
   query names its ref). This decides R5's cost and is measured before it is chosen.
2. **jj or gix.** `jj-lib` brings a working-copy model and operation log close to R2 and R3; `gix`
   alone is smaller. Either keeps git out of process.
3. **Refs on the wire.** `read_source` and `propose` gain a ref, defaulting to the daemon's
   checkout, as a minor schema change.

## Falsifiers

| # | Claim | Test |
|---|---|---|
| F1 | a restart keeps accepted proposals | daemon with a checkout; propose; kill -9 between COMMIT and materialization; restart; `read_source` and the directory both hold the proposal |
| F2 | an edit made while the daemon was down is kept | edit the directory with the daemon stopped; restart; the edit is a proposal on the ref |
| F3 | concurrent change on both sides is never overwritten | edit the directory and propose to the ref; reconcile reports a conflict; both versions stay retrievable |
| F4 | branches share storage | two refs that differ by one file; `source_blobs` grows by one blob |
| F5 | no git subprocess | no `Command::new("git")` in the daemon; a smell rule enforces it |
| F6 | git round trip | import a git commit, export the ref; the git tree ids match |
