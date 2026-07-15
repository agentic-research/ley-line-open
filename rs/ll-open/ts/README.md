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
  - `hcl` — tree-sitter-hcl (covers Terraform `.tf` / `.tfvars`)
  - `rust` — tree-sitter-rust
  - `proto` — tree-sitter-proto (proto2 + proto3)
  - `javascript` — tree-sitter-javascript
  - `typescript` — tree-sitter-typescript (TSX grammar; covers `.ts` + `.tsx`)
  - `sql` — tree-sitter-sequel (DerekStride grammar)
  - `bash` — tree-sitter-bash
  - `java` — tree-sitter-java
  - `c` — tree-sitter-c (`.c` / `.h`)
  - `cpp` — tree-sitter-cpp
  - `toml` — tree-sitter-toml-ng *(parse/validate only — no def/ref algebra)*
  - `dockerfile` — tree-sitter-containerfile (`Dockerfile` / `Containerfile` / `.dockerfile`; *parse/validate only*)
  - `ruby` — tree-sitter-ruby
  - `php` — tree-sitter-php
  - `kotlin` — tree-sitter-kotlin-ng
  - `swift` — tree-sitter-swift
  - `scala` — tree-sitter-scala
  - `csharp` — tree-sitter-c-sharp
  - `css` — tree-sitter-css *(parse/validate only)*
  - `groovy` — tree-sitter-groovy (`.groovy` + `Jenkinsfile`)
  - `lua` — tree-sitter-lua

  The Tier 1+2 bulk (`sql` … `lua`, bead `ley-line-open-46ae48`) ships
  parse → `_ast` plus validate (ERROR/MISSING enumeration via the daemon
  `validate` op). Def/ref extraction (Tier 3) is separate work; `cue` is
  not registered — the only crates.io grammar crate pins the
  pre-LanguageFn tree-sitter ~0.20 ABI.

## Feature flags

- Language features above — enable the corresponding tree-sitter grammar.
- `pyproject` — parse `pyproject.toml` using uv crates (PEP 508 dependency specifiers, PEP 440 versions, package normalization). Projects `/project/*`, `/deps/*`, `/{group}/*`, `/optional/{extra}/*`.

## Standalone crate

`leyline-ts` has no dependency on `leyline-fs`. It reads/writes SQLite directly via `rusqlite`. Output is a serialized `.db` file loadable into an arena.
