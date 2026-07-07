//! `leyline doctor` — check the environment for bundled LSP language
//! servers.
//!
//! Bead `ley-line-open-661727` (chain finale, v0.5.7): LLO skips
//! gracefully at runtime when a language server isn't on PATH (v0.5.2+
//! surfaces the reason via `EnrichmentStats.skipped`), but that's a
//! runtime-only signal. Operators + install scripts (mache, cloister)
//! benefit from a pre-flight command that reports which languages
//! will work end-to-end and which will fall back to tree-sitter-only.
//!
//! Exit code: 0 when every bundled language's server is on PATH; 1 when
//! any are missing (unless `--allow-missing`).

use std::fmt::Write as _;

use anyhow::Result;
use serde::Serialize;

/// One row in the doctor's language-server report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorRow {
    /// LSP language ID (e.g. "rust", "go", "python").
    pub language: String,
    /// The bundled server command (e.g. "rust-analyzer", "gopls").
    pub server_cmd: String,
    /// Resolved absolute path if on PATH; `None` if not found.
    pub resolved_path: Option<String>,
    /// One-line "how to install this server" hint, populated only when
    /// the server is missing. Static per language; consumers can pin
    /// them per platform if they want smarter guidance.
    pub install_hint: Option<String>,
}

/// Run `leyline doctor`. Returns the rows plus a bool: `all_present`.
pub fn run_doctor(json: bool, allow_missing: bool) -> Result<()> {
    let rows = collect_rows();
    let all_present = rows.iter().all(|r| r.resolved_path.is_some());

    if json {
        // Machine-readable output for cloister / mache install scripts.
        let out = serde_json::json!({
            "ok": all_present,
            "languages": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_human_table(&rows);
    }

    if !all_present && !allow_missing {
        anyhow::bail!("some bundled LSP servers are missing; pass --allow-missing to warn only");
    }

    Ok(())
}

/// Build the per-language rows by walking `leyline_lsp::languages` and
/// resolving each `server_cmd` against PATH via `which::which`.
fn collect_rows() -> Vec<DoctorRow> {
    use leyline_lsp::languages::LSP_LANGUAGES;

    let mut rows = Vec::new();
    for lang in LSP_LANGUAGES {
        let Some((server_cmd, _args)) = lang.server else {
            // No bundled server for this language — tree-sitter-only
            // fallback. Not a doctor concern.
            continue;
        };
        let resolved = which::which(server_cmd)
            .ok()
            .map(|p| p.display().to_string());
        let install_hint = if resolved.is_none() {
            install_hint_for(server_cmd)
        } else {
            None
        };
        rows.push(DoctorRow {
            language: lang.id.to_string(),
            server_cmd: server_cmd.to_string(),
            resolved_path: resolved,
            install_hint,
        });
    }
    rows
}

/// One-line install hint per bundled server. Deliberately not
/// platform-branching — the hint names the tool, operators know their
/// package manager. When a real platform-aware install-hint pattern
/// emerges (via cloister install scripts or user complaints), split
/// into per-OS variants.
fn install_hint_for(server_cmd: &str) -> Option<String> {
    let hint = match server_cmd {
        "rust-analyzer" => "rustup component add rust-analyzer",
        "gopls" => "go install golang.org/x/tools/gopls@latest",
        "pyright-langserver" => "npm install -g pyright",
        "typescript-language-server" => "npm install -g typescript-language-server typescript",
        "clangd" => "brew install llvm  # or apt install clangd",
        "jdtls" => {
            "brew install jdtls  # or download from https://github.com/eclipse/eclipse.jdt.ls"
        }
        "zls" => "brew install zls  # or download from https://github.com/zigtools/zls",
        _ => return None,
    };
    Some(hint.to_string())
}

/// Human-readable table. Fixed-width for terminal legibility.
fn print_human_table(rows: &[DoctorRow]) {
    // Two blocks: found + missing. Missing goes second so operators see
    // the actionable list at the bottom (last thing scrolled).
    let (found, missing): (Vec<_>, Vec<_>) = rows.iter().partition(|r| r.resolved_path.is_some());

    if !found.is_empty() {
        println!("Bundled LSP servers (found):");
        for r in &found {
            let path = r.resolved_path.as_deref().unwrap_or("");
            println!("  ✅ {:<24} {:<32} ({})", r.server_cmd, path, r.language);
        }
        println!();
    }

    if !missing.is_empty() {
        println!("Bundled LSP servers (missing — tree-sitter-only fallback):");
        for r in &missing {
            let hint = r.install_hint.as_deref().unwrap_or("(no install hint)");
            let mut line = String::new();
            let _ = write!(
                line,
                "  ❌ {:<24} not on PATH                    ({})",
                r.server_cmd, r.language
            );
            println!("{line}");
            println!("     → {hint}");
        }
        println!();
        println!(
            "Missing servers ⇒ LLO enrichment for those languages falls back to \
             tree-sitter-only (documentSymbol-shaped only; no hover/def/refs). \
             `stats.skipped` on the daemon's enrich response names the specific \
             cause per language."
        );
    } else {
        println!("All bundled LSP servers are on PATH. ✨");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_hint_covers_all_bundled_servers() {
        use leyline_lsp::languages::LSP_LANGUAGES;
        for lang in LSP_LANGUAGES {
            let Some((server_cmd, _)) = lang.server else {
                continue;
            };
            assert!(
                install_hint_for(server_cmd).is_some(),
                "bundled server '{server_cmd}' (lang '{}') must have an install hint; \
                 add it to install_hint_for()",
                lang.id
            );
        }
    }

    #[test]
    fn collect_rows_returns_one_row_per_bundled_language() {
        let rows = collect_rows();
        use leyline_lsp::languages::LSP_LANGUAGES;
        let expected = LSP_LANGUAGES.iter().filter(|l| l.server.is_some()).count();
        assert_eq!(
            rows.len(),
            expected,
            "collect_rows should emit exactly one row per bundled language"
        );
    }
}
