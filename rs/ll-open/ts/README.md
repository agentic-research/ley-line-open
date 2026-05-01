# leyline-ts

Tree-sitter AST projection and bidirectional splice.

## What's here

- **Parse** — feed source bytes + language → walk the CST → write `nodes` table + `_ast` table (byte ranges, node kinds) + `_source` table (original source text).
- **Splice** — edit an AST node's text content → patch the original source at the tracked byte range → re-parse → atomically update all tables. Batch splice handles multiple edits with byte-range overlap detection.
- **Reproject** — after splice, re-parse the patched source and replace the old AST. Syntax errors are attributed to the responsible node.
- **Languages** — feature-gated grammars:
  - `html` — tree-sitter-html *(default)*
  - `markdown` — tree-sitter-md *(default)*
  - `json` — tree-sitter-json *(default)*
  - `yaml` — tree-sitter-yaml *(default)*
  - `go` — tree-sitter-go
  - `python` — tree-sitter-python
  - `elixir` — tree-sitter-elixir

## Feature flags

- `html`, `markdown`, `json`, `yaml`, `go`, `python`, `elixir` — enable the corresponding tree-sitter grammar.
- `pyproject` — parse `pyproject.toml` using uv crates (PEP 508 dependency specifiers, PEP 440 versions, package normalization). Projects `/project/*`, `/deps/*`, `/{group}/*`, `/optional/{extra}/*`.

## Standalone crate

`leyline-ts` has no dependency on `leyline-fs`. It reads/writes SQLite directly via `rusqlite`. Output is a serialized `.db` file loadable into an arena.
