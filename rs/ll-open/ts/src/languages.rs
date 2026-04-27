//! Language registry for tree-sitter grammars.

use anyhow::{Result, bail};
use tree_sitter::Language;

/// Supported tree-sitter languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsLanguage {
    #[cfg(feature = "html")]
    Html,
    #[cfg(feature = "markdown")]
    Markdown,
    #[cfg(feature = "json")]
    Json,
    #[cfg(feature = "yaml")]
    Yaml,
    #[cfg(feature = "go")]
    Go,
    #[cfg(feature = "python")]
    Python,
    #[cfg(feature = "elixir")]
    Elixir,
}

impl TsLanguage {
    /// Get the tree-sitter `Language` object for parsing.
    pub fn ts_language(self) -> Language {
        match self {
            #[cfg(feature = "html")]
            TsLanguage::Html => tree_sitter_html::LANGUAGE.into(),
            #[cfg(feature = "markdown")]
            TsLanguage::Markdown => tree_sitter_md::LANGUAGE.into(),
            #[cfg(feature = "json")]
            TsLanguage::Json => tree_sitter_json::LANGUAGE.into(),
            #[cfg(feature = "yaml")]
            TsLanguage::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            #[cfg(feature = "go")]
            TsLanguage::Go => tree_sitter_go::LANGUAGE.into(),
            #[cfg(feature = "python")]
            TsLanguage::Python => tree_sitter_python::LANGUAGE.into(),
            #[cfg(feature = "elixir")]
            TsLanguage::Elixir => tree_sitter_elixir::LANGUAGE.into(),
        }
    }

    /// Return the canonical language name string.
    pub fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "html")]
            TsLanguage::Html => "html",
            #[cfg(feature = "markdown")]
            TsLanguage::Markdown => "markdown",
            #[cfg(feature = "json")]
            TsLanguage::Json => "json",
            #[cfg(feature = "yaml")]
            TsLanguage::Yaml => "yaml",
            #[cfg(feature = "go")]
            TsLanguage::Go => "go",
            #[cfg(feature = "python")]
            TsLanguage::Python => "python",
            #[cfg(feature = "elixir")]
            TsLanguage::Elixir => "elixir",
        }
    }

    /// Parse a language name string (case-insensitive).
    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_lowercase().as_str() {
            #[cfg(feature = "html")]
            "html" => Ok(TsLanguage::Html),
            #[cfg(feature = "markdown")]
            "markdown" | "md" => Ok(TsLanguage::Markdown),
            #[cfg(feature = "json")]
            "json" => Ok(TsLanguage::Json),
            #[cfg(feature = "yaml")]
            "yaml" | "yml" => Ok(TsLanguage::Yaml),
            #[cfg(feature = "go")]
            "go" | "golang" => Ok(TsLanguage::Go),
            #[cfg(feature = "python")]
            "python" | "py" => Ok(TsLanguage::Python),
            #[cfg(feature = "elixir")]
            "elixir" | "ex" | "exs" => Ok(TsLanguage::Elixir),
            _ => bail!("unsupported language: {name}"),
        }
    }

    /// Detect language from filename (for extensionless files like Dockerfile, Makefile).
    pub fn from_filename(name: &str) -> Option<Self> {
        match name {
            #[cfg(feature = "json")]
            ".json" | "package.json" | "tsconfig.json" | "composer.json" => Some(TsLanguage::Json),
            #[cfg(feature = "yaml")]
            ".yml" | ".yaml" | "docker-compose.yml" | "docker-compose.yaml" => Some(TsLanguage::Yaml),
            #[cfg(feature = "markdown")]
            "README" | "CHANGELOG" | "CONTRIBUTING" | "LICENSE.md" => Some(TsLanguage::Markdown),
            #[cfg(feature = "python")]
            "Pipfile" => Some(TsLanguage::Python),
            _ => None,
        }
    }

    /// Detect language from file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            #[cfg(feature = "html")]
            "html" | "htm" => Some(TsLanguage::Html),
            #[cfg(feature = "markdown")]
            "md" | "markdown" => Some(TsLanguage::Markdown),
            #[cfg(feature = "json")]
            "json" => Some(TsLanguage::Json),
            #[cfg(feature = "yaml")]
            "yaml" | "yml" => Some(TsLanguage::Yaml),
            #[cfg(feature = "go")]
            "go" => Some(TsLanguage::Go),
            #[cfg(feature = "python")]
            "py" | "pyi" => Some(TsLanguage::Python),
            #[cfg(feature = "elixir")]
            "ex" | "exs" => Some(TsLanguage::Elixir),
            _ => None,
        }
    }
}
