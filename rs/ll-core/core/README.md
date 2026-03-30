# leyline-core

Arena primitives for ley-line's data plane.

## What's here

- **`ArenaHeader`** — `#[repr(C)]` bytemuck struct at offset 0 of every arena file. Tracks magic, version, active buffer index, and sequence number.
- **`Controller`** — mmap'd control block (separate file from the arena). Stores arena path, size, and generation counter. Enables hot-reload: writer bumps generation, readers detect the change.
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
