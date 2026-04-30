//! LSP language registry — the canonical "what languages does ley-line know
//! about, and how do we run a server for them" table.
//!
//! Two crates consume this:
//!
//! 1. `cli-lib::cmd_lsp` (one-shot CLI) — needs `language_id_from_ext` to
//!    populate the LSP `languageId` field in `didOpen`. The user passes
//!    `--server X` so we don't need to know the server here.
//!
//! 2. `cli-lib::daemon::lsp_pass` (long-lived enrichment pass) — needs
//!    BOTH `language_id_from_ext` and `language_server` to spawn the right
//!    server per language group.
//!
//! Splitting the table caused a real drift bug (Ruby files were silently
//! skipped because `cmd_lsp` recognized `.rb` but the daemon didn't have
//! a server for it). One table, two views, no drift.
//!
//! Languages with `server: None` are *recognized* but the daemon enrichment
//! pass logs "no server for language X" and skips. The CLI path still
//! works — the user supplies `--server` directly.

/// One row in the language registry.
pub struct LspLanguage {
    /// Canonical LSP language id (matches LSP spec values where applicable).
    pub id: &'static str,
    /// File extensions that resolve to this language.
    pub exts: &'static [&'static str],
    /// `(binary, args)` for spawning the language server, or `None` if no
    /// canonical server is bundled. The daemon skips `None` languages with
    /// a log line; the CLI path is unaffected (it takes `--server` from the
    /// command line).
    pub server: Option<(&'static str, &'static [&'static str])>,
}

/// The full table. Order matters only for `find()`-based lookups, where the
/// first match wins. Keep entries grouped by family for readability.
pub const LSP_LANGUAGES: &[LspLanguage] = &[
    // ── Languages with a known server bundled ────────────────────────────
    LspLanguage {
        id: "go",
        exts: &["go"],
        server: Some(("gopls", &["serve"])),
    },
    LspLanguage {
        id: "python",
        exts: &["py"],
        server: Some(("pyright-langserver", &["--stdio"])),
    },
    LspLanguage {
        id: "rust",
        exts: &["rs"],
        server: Some(("rust-analyzer", &[])),
    },
    LspLanguage {
        id: "typescript",
        exts: &["ts"],
        server: Some(("typescript-language-server", &["--stdio"])),
    },
    LspLanguage {
        id: "typescriptreact",
        exts: &["tsx"],
        server: Some(("typescript-language-server", &["--stdio"])),
    },
    LspLanguage {
        id: "javascript",
        exts: &["js"],
        server: Some(("typescript-language-server", &["--stdio"])),
    },
    LspLanguage {
        id: "javascriptreact",
        exts: &["jsx"],
        server: Some(("typescript-language-server", &["--stdio"])),
    },
    LspLanguage {
        id: "c",
        exts: &["c"],
        server: Some(("clangd", &[])),
    },
    LspLanguage {
        id: "cpp",
        exts: &["cpp", "cc", "cxx", "h", "hpp"],
        server: Some(("clangd", &[])),
    },
    LspLanguage {
        id: "java",
        exts: &["java"],
        server: Some(("jdtls", &[])),
    },
    LspLanguage {
        id: "zig",
        exts: &["zig"],
        server: Some(("zls", &[])),
    },
    // ── Recognized but no bundled server ─────────────────────────────────
    // CLI path works (user supplies --server); daemon enrichment skips
    // with a log message until a server is added here.
    LspLanguage { id: "ruby",        exts: &["rb"],          server: None },
    LspLanguage { id: "elixir",      exts: &["ex", "exs"],   server: None },
    LspLanguage { id: "lua",         exts: &["lua"],         server: None },
    LspLanguage { id: "shellscript", exts: &["sh", "bash"],  server: None },
    LspLanguage { id: "css",         exts: &["css"],         server: None },
    LspLanguage { id: "html",        exts: &["html", "htm"], server: None },
    LspLanguage { id: "json",        exts: &["json"],        server: None },
    LspLanguage { id: "yaml",        exts: &["yaml", "yml"], server: None },
    LspLanguage { id: "toml",        exts: &["toml"],        server: None },
    LspLanguage { id: "markdown",    exts: &["md"],          server: None },
    LspLanguage { id: "swift",       exts: &["swift"],       server: None },
    LspLanguage { id: "kotlin",      exts: &["kt", "kts"],   server: None },
    LspLanguage { id: "terraform",   exts: &["tf", "hcl"],   server: None },
];

/// Look up the language server invocation for an LSP language ID. Returns
/// `None` if the language is unknown OR known but unbundled (caller should
/// log/skip rather than panic).
pub fn language_server(lang: &str) -> Option<(&'static str, &'static [&'static str])> {
    LSP_LANGUAGES
        .iter()
        .find(|l| l.id == lang)
        .and_then(|l| l.server)
}

/// Infer the LSP language ID from a file extension.
pub fn language_id_from_ext(ext: &str) -> Option<&'static str> {
    LSP_LANGUAGES
        .iter()
        .find(|l| l.exts.contains(&ext))
        .map(|l| l.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_extension_appears_under_two_languages() {
        // Scale-problem pin. language_id_from_ext does a `find()`
        // over LSP_LANGUAGES — first match wins. If a future entry
        // accidentally re-registered an existing ext (e.g. adding
        // "node" with exts=["js"]), the second entry would be
        // silently shadowed and every .js file would still resolve
        // to "javascript" — but the new entry would be invisible.
        // Worse: file-of-truth drift between this table and the
        // ts::TsLanguage table. Pin uniqueness across the whole
        // registry.
        let mut seen = std::collections::HashMap::<&str, &str>::new();
        for lang in LSP_LANGUAGES {
            for ext in lang.exts {
                if let Some(prev_id) = seen.insert(ext, lang.id) {
                    panic!(
                        "extension `{ext}` registered under both `{prev_id}` and `{}`",
                        lang.id,
                    );
                }
            }
        }
    }

    #[test]
    fn every_entry_has_id_and_at_least_one_ext() {
        for lang in LSP_LANGUAGES {
            assert!(!lang.id.is_empty(), "language id must not be empty");
            assert!(
                !lang.exts.is_empty(),
                "language `{}` must register at least one extension",
                lang.id,
            );
        }
    }

    #[test]
    fn every_ext_resolves_back_to_a_language() {
        // Each ext must round-trip: ext → language_id → at least one ext
        // in that entry's list. Catches typos like `LspLanguage { id:
        // "foo", exts: &["foo"] }` where the id and the only ext disagree.
        for lang in LSP_LANGUAGES {
            for ext in lang.exts {
                let id = language_id_from_ext(ext)
                    .unwrap_or_else(|| panic!("ext `{ext}` did not resolve"));
                let resolved = LSP_LANGUAGES
                    .iter()
                    .find(|l| l.id == id)
                    .expect("resolved id has no entry");
                assert!(
                    resolved.exts.contains(ext),
                    "ext `{ext}` resolved to `{id}` but `{id}` doesn't list `{ext}`",
                );
            }
        }
    }

    #[test]
    fn server_lookup_for_bundled_languages() {
        // Sanity: each bundled language must produce a non-empty server
        // command. Caught a regression where one entry had an empty
        // binary string in an earlier draft.
        for lang in LSP_LANGUAGES.iter().filter(|l| l.server.is_some()) {
            let (cmd, _args) = lang.server.unwrap();
            assert!(
                !cmd.is_empty(),
                "language `{}` has Some(server) but empty binary",
                lang.id,
            );
        }
    }

    #[test]
    fn unbundled_languages_resolve_id_but_not_server() {
        // Ruby: the original drift case. ext is recognized, no server.
        // This is the "documented unsupported" state.
        assert_eq!(language_id_from_ext("rb"), Some("ruby"));
        assert!(language_server("ruby").is_none());

        assert_eq!(language_id_from_ext("ex"), Some("elixir"));
        assert!(language_server("elixir").is_none());
    }

    #[test]
    fn known_extension_lookups() {
        assert_eq!(language_id_from_ext("rs"),  Some("rust"));
        assert_eq!(language_id_from_ext("go"),  Some("go"));
        assert_eq!(language_id_from_ext("py"),  Some("python"));
        assert_eq!(language_id_from_ext("ts"),  Some("typescript"));
        assert_eq!(language_id_from_ext("tsx"), Some("typescriptreact"));
        assert_eq!(language_id_from_ext("hpp"), Some("cpp"));
    }

    #[test]
    fn unknown_extension_returns_none() {
        assert_eq!(language_id_from_ext("foobar"), None);
        assert_eq!(language_id_from_ext(""),       None);
        assert_eq!(language_id_from_ext("xyz"),    None);
    }

    #[test]
    fn typescript_family_shares_one_server() {
        let ts = language_server("typescript").unwrap();
        assert_eq!(ts.0, "typescript-language-server");
        assert_eq!(ts, language_server("typescriptreact").unwrap());
        assert_eq!(ts, language_server("javascript").unwrap());
        assert_eq!(ts, language_server("javascriptreact").unwrap());
    }
}
