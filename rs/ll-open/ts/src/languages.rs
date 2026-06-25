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
    /// HashiCorp Configuration Language. Same grammar covers Terraform
    /// (.tf / .tfvars / .hcl) — there is no separate Terraform grammar;
    /// the `tree-sitter-hcl` crate handles both.
    #[cfg(feature = "hcl")]
    Hcl,
    #[cfg(feature = "rust")]
    Rust,
    /// Protobuf (.proto). The `tree-sitter-proto` grammar handles
    /// both proto2 and proto3 syntax.
    #[cfg(feature = "proto")]
    Proto,
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
            #[cfg(feature = "hcl")]
            TsLanguage::Hcl => tree_sitter_hcl::LANGUAGE.into(),
            #[cfg(feature = "rust")]
            TsLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
            #[cfg(feature = "proto")]
            TsLanguage::Proto => tree_sitter_proto::LANGUAGE.into(),
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
            #[cfg(feature = "hcl")]
            // Canonical name is "hcl" — the grammar covers Terraform too,
            // but consumers spell it "hcl" in the language tag because one
            // grammar can't sensibly answer to two names without picking
            // one as canonical. Terraform spellings are aliased in
            // `from_name` + `from_extension` below.
            TsLanguage::Hcl => "hcl",
            #[cfg(feature = "rust")]
            TsLanguage::Rust => "rust",
            #[cfg(feature = "proto")]
            TsLanguage::Proto => "proto",
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
            #[cfg(feature = "hcl")]
            "hcl" | "terraform" | "tf" | "tfvars" => Ok(TsLanguage::Hcl),
            #[cfg(feature = "rust")]
            "rust" | "rs" => Ok(TsLanguage::Rust),
            #[cfg(feature = "proto")]
            "proto" | "protobuf" => Ok(TsLanguage::Proto),
            _ => bail!("unsupported language: {name}"),
        }
    }

    /// Detect language from filename (for extensionless files like Dockerfile, Makefile).
    pub fn from_filename(name: &str) -> Option<Self> {
        match name {
            #[cfg(feature = "json")]
            ".json" | "package.json" | "tsconfig.json" | "composer.json" => Some(TsLanguage::Json),
            #[cfg(feature = "yaml")]
            ".yml" | ".yaml" | "docker-compose.yml" | "docker-compose.yaml" => {
                Some(TsLanguage::Yaml)
            }
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
            // .tf is the dominant Terraform spelling; .tfvars is the
            // variables-only file; .hcl is the vanilla HCL extension
            // (Nomad, Vault, Packer, Consul Template). One grammar
            // covers all three.
            #[cfg(feature = "hcl")]
            "tf" | "tfvars" | "hcl" => Some(TsLanguage::Hcl),
            #[cfg(feature = "rust")]
            "rs" => Some(TsLanguage::Rust),
            #[cfg(feature = "proto")]
            "proto" => Some(TsLanguage::Proto),
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
            assert_eq!(
                TsLanguage::from_extension("markdown"),
                Some(TsLanguage::Markdown)
            );
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
        #[cfg(feature = "hcl")]
        {
            assert_eq!(TsLanguage::from_extension("tf"), Some(TsLanguage::Hcl));
            assert_eq!(TsLanguage::from_extension("tfvars"), Some(TsLanguage::Hcl));
            assert_eq!(TsLanguage::from_extension("hcl"), Some(TsLanguage::Hcl));
            // Case-insensitive: Terraform configs occasionally appear as
            // .TF on case-preserving file systems (Windows shares,
            // mismatched git config).
            assert_eq!(TsLanguage::from_extension("TF"), Some(TsLanguage::Hcl));
        }
        #[cfg(feature = "proto")]
        {
            assert_eq!(TsLanguage::from_extension("proto"), Some(TsLanguage::Proto));
            // Case-insensitive: Windows-generated .proto files occasionally
            // land as .PROTO on case-preserving filesystems.
            assert_eq!(TsLanguage::from_extension("PROTO"), Some(TsLanguage::Proto));
        }

        // Unknown extension → None, never default to one language.
        assert_eq!(TsLanguage::from_extension("unknown_lang_ext"), None);
        assert_eq!(TsLanguage::from_extension(""), None);
    }

    /// Parses a tiny proto3 fragment end-to-end to verify the
    /// `tree-sitter-proto` grammar is actually wired through
    /// `ts_language()`. Fragment uses message / field / enum / service —
    /// the primitives any real `.proto` has. If the grammar is broken
    /// or mis-wired, this fails at the `parse()` call.
    #[cfg(feature = "proto")]
    #[test]
    fn proto_parses_minimal_proto3_fragment() {
        use tree_sitter::Parser;
        let mut parser = Parser::new();
        parser
            .set_language(&TsLanguage::Proto.ts_language())
            .expect("set_language proto");
        let src = r#"
syntax = "proto3";

package example.v1;

message Region {
  string name = 1;
  int32 zone_count = 2;
  repeated string availability_zones = 3;
}

enum Tier {
  TIER_UNSPECIFIED = 0;
  TIER_FREE = 1;
  TIER_PRO = 2;
}

service Regions {
  rpc Get (Region) returns (Region);
}
"#;
        let tree = parser
            .parse(src, None)
            .expect("parse() must return a tree for valid proto3");
        let root = tree.root_node();
        assert!(
            !root.has_error(),
            "valid proto3 fragment must parse without errors; root: {root:?}",
        );
        assert!(
            root.named_child_count() > 0,
            "root must have named children (syntax / package / message / enum / service); got 0",
        );
    }

    /// Pin the `proto` / `protobuf` from_name aliases — mache and other
    /// consumers may pass either spelling depending on convention.
    #[cfg(feature = "proto")]
    #[test]
    fn proto_aliases_all_resolve_to_one_language() {
        for spelling in ["proto", "protobuf", "Proto", "PROTOBUF"] {
            let lang = TsLanguage::from_name(spelling)
                .unwrap_or_else(|e| panic!("from_name({spelling:?}): {e}"));
            assert_eq!(
                lang,
                TsLanguage::Proto,
                "spelling {spelling:?} must resolve to TsLanguage::Proto",
            );
        }
    }

    /// Pin Terraform-spelling aliases on the `from_name` path. Mache and
    /// other consumers pass the language tag explicitly (`--lang
    /// terraform`); the grammar covers all four spellings, so all four
    /// must round-trip to the same TsLanguage variant.
    #[cfg(feature = "hcl")]
    #[test]
    fn hcl_aliases_all_resolve_to_one_language() {
        for spelling in ["hcl", "terraform", "tf", "tfvars", "HCL", "Terraform"] {
            let lang = TsLanguage::from_name(spelling)
                .unwrap_or_else(|e| panic!("from_name({spelling:?}): {e}"));
            assert_eq!(
                lang,
                TsLanguage::Hcl,
                "spelling {spelling:?} must resolve to TsLanguage::Hcl",
            );
        }
    }

    /// Parses a tiny Terraform fragment end-to-end to verify the grammar
    /// is actually wired through `ts_language()`. The fragment uses the
    /// resource / variable / provider primitives that any real .tf file
    /// has; if the grammar is broken or mis-wired, this fails at the
    /// `parse()` call.
    #[cfg(feature = "hcl")]
    #[test]
    fn hcl_parses_minimal_terraform_fragment() {
        use tree_sitter::Parser;
        let mut parser = Parser::new();
        parser
            .set_language(&TsLanguage::Hcl.ts_language())
            .expect("set_language hcl");
        let src = r#"
terraform {
  required_version = ">= 1.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "region" {
  type    = string
  default = "us-west-2"
}

resource "aws_s3_bucket" "logs" {
  bucket = "example-logs"
  tags = {
    Environment = "prod"
  }
}
"#;
        let tree = parser
            .parse(src, None)
            .expect("parse() must return a tree for valid Terraform");
        let root = tree.root_node();
        assert!(
            !root.has_error(),
            "valid Terraform fragment must parse without errors; root: {root:?}",
        );
        assert!(
            root.named_child_count() > 0,
            "root must have named children (block / variable / resource); got 0",
        );
    }
}
