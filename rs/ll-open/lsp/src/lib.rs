//! LSP client that projects language server data into the `nodes` table.
//!
//! Spawns a language server (gopls, pyright, rust-analyzer) over stdio,
//! collects symbols + diagnostics, and merges them with existing tree-sitter
//! AST data in a SQLite database.
//!
//! The projection layout:
//! ```text
//! /symbols/                    documentSymbol hierarchy
//! /symbols/MyClass             kind=class, record=detail
//! /symbols/MyClass/__init__    kind=method
//! /diagnostics/                publishDiagnostics
//! /diagnostics/0               record=message, severity in name
//! /references/{symbol}         textDocument/references
//! ```

pub mod client;
pub mod project;
pub mod protocol;
