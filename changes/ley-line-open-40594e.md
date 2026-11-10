Section: Fixed

- **`--reset-arena` removes a crashed reset's staging file** (bead
  `ley-line-open-40594e`). A reset stages the fresh control block at
  `.<ctrl>.reinit.<pid>` and renames it into place; a crash between the two
  left the file, and later resets, staging under their own pid, never
  removed it. Every reset now removes `.<ctrl>.reinit.<digits>` siblings
  while it holds the lease exclusively, whatever the block's version
  (`leyline_core::control::remove_stale_reinit_staging`).
