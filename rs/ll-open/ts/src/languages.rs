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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_extension_pins_alias_sets() {
        // Scale-problem pin. The extension-set choices are load-bearing
        // for registry-style ingest: a 50k-file Aports clone uses
        // "yaml" but third-party tooling/conventions may write "yml".
        // Dropping either alias (or losing case-insensitivity) would
        // silently skip files at parse time. Existing parse paths
        // exercise the function but never pin the alias mappings
        // directly. Pin every alias set.
        #[cfg(feature = "yaml")]
        {
            assert_eq!(TsLanguage::from_extension("yaml"), Some(TsLanguage::Yaml));
            assert_eq!(TsLanguage::from_extension("yml"), Some(TsLanguage::Yaml));
            // Case-insensitive: APK packages occasionally land .YAML.
            assert_eq!(TsLanguage::from_extension("YAML"), Some(TsLanguage::Yaml));
        }
        #[cfg(feature = "markdown")]
        {
            assert_eq!(TsLanguage::from_extension("md"), Some(TsLanguage::Markdown));
            assert_eq!(TsLanguage::from_extension("markdown"), Some(TsLanguage::Markdown));
        }
        #[cfg(feature = "json")]
        assert_eq!(TsLanguage::from_extension("json"), Some(TsLanguage::Json));
        #[cfg(feature = "html")]
        {
            assert_eq!(TsLanguage::from_extension("html"), Some(TsLanguage::Html));
            assert_eq!(TsLanguage::from_extension("htm"), Some(TsLanguage::Html));
        }
        #[cfg(feature = "go")]
        assert_eq!(TsLanguage::from_extension("go"), Some(TsLanguage::Go));
        #[cfg(feature = "python")]
        {
            assert_eq!(TsLanguage::from_extension("py"), Some(TsLanguage::Python));
            assert_eq!(TsLanguage::from_extension("pyi"), Some(TsLanguage::Python));
        }
        #[cfg(feature = "elixir")]
        {
            assert_eq!(TsLanguage::from_extension("ex"), Some(TsLanguage::Elixir));
            assert_eq!(TsLanguage::from_extension("exs"), Some(TsLanguage::Elixir));
        }

        // Unknown extension → None, never default to one language.
        assert_eq!(TsLanguage::from_extension("unknown_lang_ext"), None);
        assert_eq!(TsLanguage::from_extension(""), None);
    }
}
