Section: Fixed

- **Identity hardening: absence is stated, not encoded as zero, and the
  fold laws are checked as properties** (bead `ley-line-open-80f037`,
  findings F3, F8, F9 and F10 of `ley-line-open-619a99`).
  - `status` adds `root_published` (`StatusResponse.rootPublished @12`,
    additive; Go `StatusResponse.RootPublished`). While it is `false`,
    `current_root` is the 64-zero sentinel of the control block, which
    `daemon.capnp` now documents. `Controller::published_root()` returns
    `None` for that sentinel, and the daemon's freshness check uses it.
  - `leyline daemon --reset-arena` fails if it cannot open the controller
    or zero its root. It used to discard the error and could then warm-start
    from the root it was asked to reset.
  - A head that stamps only one of `treeRoot` and `parserId` is malformed:
    `verify-head` and the chaining parse refuse it instead of reading it as
    a pre-ADR-0040 head, and the head writer refuses to produce one.
  - The interning tables (`names`, `kinds`, `dirs`, `files`) are
    append-only in the database: triggers abort any DELETE and any UPDATE
    that changes a row's id, so a `file_id` can no longer be reused to
    re-bind a dead file's nid range. A rename (same id) still works. Arenas
    gain the triggers on their next schema pass.
  - proptest properties cover `PartitionSpec` injectivity (ordered domain)
    and set canonicality (set domains), and `treeRoot` permutation
    invariance and sensitivity to one path byte, one content byte, one mode
    and one path boundary, over paths holding `/`, 0x1f and newline.
  - The malformed-`parentHash`-as-ZERO read (F3, first site) was already
    closed by the v2 head claim (`ley-line-open-b68fa6`), whose test
    `a_malformed_hash_field_is_an_error` covers it.
