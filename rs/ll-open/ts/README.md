# leyline-ts

Tree-sitter AST projection and bidirectional splice.

## What's here

- **Parse** — feed source bytes + language → walk the CST → write `nodes` table + `_ast` table (byte ranges, node kinds) + `_source` table (original source text).
- **Splice** — edit an AST node's text content → patch the original source at the tracked byte range → re-parse → atomically update all tables. Batch splice handles multiple edits with byte-range overlap detection.
- **Reproject** — after splice, re-parse the patched source and replace the old AST. Syntax errors are attributed to the responsible node.
- **Languages** — feature-gated grammars:
  - `html` — tree-sitter-html
  - `markdown` — tree-sitter-md
  - `json` — tree-sitter-json
  - `yaml` — tree-sitter-yaml
  - `go` — tree-sitter-go
  - `python` — tree-sitter-python

## Feature flags

- `html`, `markdown`, `json`, `yaml`, `go`, `python` — enable the corresponding tree-sitter grammar.
- `pyproject` — parse `pyproject.toml` using uv crates (PEP 508 dependency specifiers, PEP 440 versions, package normalization). Projects `/project/*`, `/deps/*`, `/{group}/*`, `/optional/{extra}/*`.

## Standalone crate

`leyline-ts` has no dependency on `leyline-fs`. It reads/writes SQLite directly via `rusqlite`. Output is a serialized `.db` file loadable into an arena.
