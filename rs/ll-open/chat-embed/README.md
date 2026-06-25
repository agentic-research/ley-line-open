# leyline-chat-embed

Semantic search over Claude Code chat session databases (mache ingest `claude-chats` output) via fastembed / MiniLM. CLI binary.

**License:** AGPL-3.0-or-later.

## What's here

- **`bin/chat-embed`** — embed-and-query CLI over a SQLite chat database produced by mache's `claude-chats` ingest path. Subcommands handle:
  - building embeddings for un-embedded messages
  - top-K semantic search across embedded messages
  - cross-session retrieval at the message granularity

## Why this exists

mache ingests Claude Code chat transcripts into SQLite for archaeology + recovery. The raw transcript text is searchable by grep / FTS, but neither captures semantic similarity ("find messages discussing X" vs "find messages containing X"). This binary adds a dense-vector retrieval surface on top of the chat database, using the same fastembed model the rest of LLO's `vec_search` op uses (single embedding model across the substrate).

The chat-side semantic surface is kept separate from LLO's main `vec_search` (which operates over source code) because the corpora, ingestion cadence, and storage shape are different: chat databases are append-only conversation logs; the source-code substrate is the Σ Merkle-CAS arena.

## Used by

- Manual / scripted use during agent-development debugging — "find prior conversations about HDC substrate decisions" type queries.
- Not on the daemon op surface today (binary-only).

## Dependencies

- `fastembed` (MiniLM-L6, 384-dim) — same model as LLO's `vec_search`.
- `rusqlite` for the chat database I/O.
