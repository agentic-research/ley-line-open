# leyline-core

Arena primitives for ley-line's data plane.

## What's here

- **`ArenaHeader`** — `#[repr(C)]` bytemuck struct at offset 0 of every arena file. Tracks magic, version, active buffer index, and sequence number.
- **`Controller`** — mmap'd control block (separate file from the arena; `.ctrl` VERSION 3). Stores arena path, size, and `current_root`, published under a seqlock: the writer marks the sequence odd before writing the payload and even after, and readers retry while it is odd or when it moved under their copy, so a read never returns a half-published root. Hot-reload: readers poll `current_root` and swap when it changes.
- **`create_arena()`** — allocate and initialize the `[Header][Buf0][Buf1]` layout.
- **`write_to_arena()`** — write SQLite bytes into the inactive buffer and flip the active index.

## Layout

```
Offset 0                        4096              4096 + buf_size
┌──────────┬───────────────────┬───────────────────┐
│  Header  │     Buffer 0      │     Buffer 1      │
│ (4096 B) │  (SQLite .db)     │  (SQLite .db)     │
└──────────┴───────────────────┴───────────────────┘
```

Each buffer holds a complete serialized SQLite database. The header's `active_buffer` field (0 or 1) tells readers which one is current.
