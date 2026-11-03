Section: Added

- **A property test gates merge commutativity** (bead
  `ley-line-open-238c85`). Two stale proposals whose edits are disjoint in
  their shared base were suspected of landing different bytes depending on
  arrival order, through Myers re-alignment on repeated lines. Over 10,000
  cases on a two-line alphabet (fixed seed `0x238c85`), both orders always
  agree: the same bytes, or a refusal in both. The test stays as a
  regression gate. ADR-0045 T5 now states the rule over bytes: a merge keeps
  what both sides' bytes determine, so on a repeated line two edits disjoint
  in base coordinates can align to one (for the base `a\na\n`, deleting
  either line proposes `a\n`, and the merge lands `a\n`).
