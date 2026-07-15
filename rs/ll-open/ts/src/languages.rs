//! Language registry for tree-sitter grammars.

use anyhow::{Result, bail};
use tree_sitter::Language;

/// Closed set of κ-canonical control-flow node kinds (bead
/// `ley-line-open-46aef2`, decade `dataflow-substrate`, thread
/// `cfg-emission`). The vocabulary the CFG builder (T1.b3) emits
/// into `_cfg.block_kind`; new grammars extend `canonical_cfg_kind`
/// to map their raw CF kinds into this set.
///
/// Six of these ten (`loop_back`, `call`, `throw`, `try_enter`,
/// `try_exit`, `case`) are **builder-emitted** kinds — no raw
/// grammar node maps directly at the κ level (they are synthesized
/// from lower-level structure by the CFG builder). Named here so the
/// closed-set discipline (ADR-0027 §6) is enforced by pin:
/// `canonical_cfg_kind` results MUST be members of this array, and
/// the array MUST cover every kind T1.b3's builder emits.
///
/// Order is stable — consumers may index into it, and pin tests
/// (`kappa_cfg_closed_set`) rely on the exact contents.
pub const CFG_CANONICAL_KINDS: [&str; 10] = [
    "branch",
    "loop_head",
    "loop_back",
    "call",
    "return",
    "throw",
    "try_enter",
    "try_exit",
    "switch",
    "case",
];

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
    /// JavaScript (.js / .mjs / .cjs / .jsx). Wired for def/ref
    /// extraction so mache's cross-language rules see JS symbols
    /// (bead `ley-line-open-caf423`).
    #[cfg(feature = "javascript")]
    JavaScript,
    /// TypeScript (.ts / .tsx). Uses tree-sitter-typescript's TSX
    /// grammar — a strict superset that handles both plain TypeScript
    /// and TSX (TypeScript + JSX) uniformly, so one variant covers
    /// both extensions. Same bead as JavaScript (`ley-line-open-caf423`)
    /// — TS files parsed via leyline-fs's validate pass but LLO's
    /// producer had no TS arm, so every `.ts`/`.tsx` file wrote zero
    /// symbols to `node_defs` / `node_refs`.
    #[cfg(feature = "typescript")]
    TypeScript,
    // ── Tier 1+2 grammar bulk (bead ley-line-open-46ae48, parent
    // e5addb) — the 16 mache-registry languages LLO lacked at mache's
    // CGO removal. Coverage per language: Tier 1 (parse → `_ast`) +
    // Tier 2 (validate: ERROR/MISSING enumeration via the daemon
    // `validate` op). Tier 3 (def/ref extraction) is .scm query data
    // over the generic engine: java/c/cpp have it (bead
    // ley-line-open-5e21c2, `queries/<lang>/tags.scm` + κ arms below);
    // the rest return `None` from `canonical_kind` /
    // `canonical_cfg_kind` (the open-world escape: callers keep the
    // raw grammar kind) until their queries are authored. ──
    /// SQL (.sql). DerekStride/tree-sitter-sql (crate
    /// `tree-sitter-sequel`) — the same grammar mache vendored through
    /// smacker/go-tree-sitter before its CGO removal, so `_ast` node
    /// kinds match what mache's sql preset schema was built against.
    #[cfg(feature = "sql")]
    Sql,
    /// Bash / POSIX shell (.sh / .bash).
    #[cfg(feature = "bash")]
    Bash,
    /// Java (.java).
    #[cfg(feature = "java")]
    Java,
    /// C (.c / .h). `.h` maps here, mirroring mache's registry row;
    /// C++-spelled headers (.hpp/.hxx/.hh) map to `Cpp`.
    #[cfg(feature = "c")]
    C,
    /// C++ (.cpp / .cc / .cxx / .hpp / .hxx / .hh).
    #[cfg(feature = "cpp")]
    Cpp,
    /// TOML (.toml). Config/data language: no def/ref algebra —
    /// parse/validate only BY DESIGN.
    #[cfg(feature = "toml")]
    Toml,
    /// Dockerfile / Containerfile (.dockerfile + the extensionless
    /// `Dockerfile` / `Containerfile` filenames via `from_filename`).
    /// Grammar: wharflab/tree-sitter-containerfile — covers both
    /// spellings; camdencheek's dockerfile crate is stuck on the
    /// pre-LanguageFn ABI. Config/data language: no def/ref algebra —
    /// parse/validate only BY DESIGN.
    #[cfg(feature = "dockerfile")]
    Dockerfile,
    /// Ruby (.rb).
    #[cfg(feature = "ruby")]
    Ruby,
    /// PHP (.php). Uses `LANGUAGE_PHP` — the full grammar with
    /// `<?php` tags and interleaved HTML, matching how PHP files
    /// exist on disk. `LANGUAGE_PHP_ONLY` (tagless) is not wired.
    #[cfg(feature = "php")]
    Php,
    /// Kotlin (.kt / .kts). Grammar: tree-sitter-grammars fork
    /// (crate `tree-sitter-kotlin-ng`); fwcd's original crate is on
    /// the pre-LanguageFn ABI.
    #[cfg(feature = "kotlin")]
    Kotlin,
    /// Swift (.swift). Grammar: alex-pinkus/tree-sitter-swift.
    #[cfg(feature = "swift")]
    Swift,
    /// Scala (.scala / .sc).
    #[cfg(feature = "scala")]
    Scala,
    /// C# (.cs). Crate `tree-sitter-c-sharp`; canonical name "csharp".
    #[cfg(feature = "csharp")]
    CSharp,
    /// CSS (.css). Stylesheet: no def/ref algebra — parse/validate
    /// only BY DESIGN.
    #[cfg(feature = "css")]
    Css,
    /// Groovy (.groovy + the extensionless `Jenkinsfile` filename via
    /// `from_filename`, mirroring mache's sentinel).
    #[cfg(feature = "groovy")]
    Groovy,
    /// Lua (.lua). Grammar: tree-sitter-grammars/tree-sitter-lua.
    #[cfg(feature = "lua")]
    Lua,
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
            #[cfg(feature = "javascript")]
            TsLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            #[cfg(feature = "typescript")]
            // TSX grammar covers both `.ts` and `.tsx` — one grammar,
            // one variant, mirrors how the JavaScript variant covers
            // .js/.jsx/.mjs/.cjs. LANGUAGE_TYPESCRIPT is the JSX-less
            // sibling; we don't wire a separate variant for it because
            // the TSX grammar parses plain TypeScript cleanly.
            TsLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
            #[cfg(feature = "sql")]
            TsLanguage::Sql => tree_sitter_sequel::LANGUAGE.into(),
            #[cfg(feature = "bash")]
            TsLanguage::Bash => tree_sitter_bash::LANGUAGE.into(),
            #[cfg(feature = "java")]
            TsLanguage::Java => tree_sitter_java::LANGUAGE.into(),
            #[cfg(feature = "c")]
            TsLanguage::C => tree_sitter_c::LANGUAGE.into(),
            #[cfg(feature = "cpp")]
            TsLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            #[cfg(feature = "toml")]
            TsLanguage::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            #[cfg(feature = "dockerfile")]
            TsLanguage::Dockerfile => tree_sitter_containerfile::LANGUAGE.into(),
            #[cfg(feature = "ruby")]
            TsLanguage::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            #[cfg(feature = "php")]
            // LANGUAGE_PHP = full PHP with <?php tags + interleaved
            // HTML; LANGUAGE_PHP_ONLY is the tagless embedded variant.
            TsLanguage::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            #[cfg(feature = "kotlin")]
            TsLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            #[cfg(feature = "swift")]
            TsLanguage::Swift => tree_sitter_swift::LANGUAGE.into(),
            #[cfg(feature = "scala")]
            TsLanguage::Scala => tree_sitter_scala::LANGUAGE.into(),
            #[cfg(feature = "csharp")]
            TsLanguage::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            #[cfg(feature = "css")]
            TsLanguage::Css => tree_sitter_css::LANGUAGE.into(),
            #[cfg(feature = "groovy")]
            TsLanguage::Groovy => tree_sitter_groovy::LANGUAGE.into(),
            #[cfg(feature = "lua")]
            TsLanguage::Lua => tree_sitter_lua::LANGUAGE.into(),
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
            #[cfg(feature = "javascript")]
            TsLanguage::JavaScript => "javascript",
            #[cfg(feature = "typescript")]
            TsLanguage::TypeScript => "typescript",
            #[cfg(feature = "sql")]
            TsLanguage::Sql => "sql",
            #[cfg(feature = "bash")]
            TsLanguage::Bash => "bash",
            #[cfg(feature = "java")]
            TsLanguage::Java => "java",
            #[cfg(feature = "c")]
            TsLanguage::C => "c",
            #[cfg(feature = "cpp")]
            TsLanguage::Cpp => "cpp",
            #[cfg(feature = "toml")]
            TsLanguage::Toml => "toml",
            #[cfg(feature = "dockerfile")]
            // Canonical name is "dockerfile" (mache's registry row);
            // "containerfile" is aliased in `from_name`.
            TsLanguage::Dockerfile => "dockerfile",
            #[cfg(feature = "ruby")]
            TsLanguage::Ruby => "ruby",
            #[cfg(feature = "php")]
            TsLanguage::Php => "php",
            #[cfg(feature = "kotlin")]
            TsLanguage::Kotlin => "kotlin",
            #[cfg(feature = "swift")]
            TsLanguage::Swift => "swift",
            #[cfg(feature = "scala")]
            TsLanguage::Scala => "scala",
            #[cfg(feature = "csharp")]
            TsLanguage::CSharp => "csharp",
            #[cfg(feature = "css")]
            TsLanguage::Css => "css",
            #[cfg(feature = "groovy")]
            TsLanguage::Groovy => "groovy",
            #[cfg(feature = "lua")]
            TsLanguage::Lua => "lua",
        }
    }

    /// κ — cross-language kind collapse (ADR-0027 / mache ADR-0023).
    ///
    /// Maps a raw tree-sitter node kind to a *canonical, language-agnostic*
    /// base kind so a consumer can query "all functions" once, without
    /// special-casing `function_declaration` (Go) vs `function_item`
    /// (Rust) vs `function_definition` (Python).
    ///
    /// The base-kind set is closed: `function`, `method`, `type`, `field`,
    /// `variable`, `constant`, `module`, `import`, `parameter`. Anything
    /// unmapped falls through to the raw kind (open-world escape hatch) —
    /// `raw_kind` is retained alongside `kind` in the `symbols` table, so
    /// language-specific rules can still discriminate on the raw grammar
    /// kind. This is the same single-source-of-truth discipline mache's
    /// `internal/lang` uses; new grammars extend the match arms here.
    ///
    /// Coverage is deliberately partial in this slice — the actively-used
    /// grammars (Go, Rust, Python) get the base collapse; other languages
    /// pass through raw until reviewed per-language.
    ///
    /// Returns `Some(canonical)` when the raw kind maps to a base kind, or
    /// `None` when it does not — the caller stores `raw_kind` as `kind` in
    /// that case (the open-world escape). Returning `Option` keeps the
    /// canonical set a closed `&'static` enum of literals without having to
    /// launder the borrowed `raw_kind` into a `'static` lifetime.
    pub fn canonical_kind(self, raw_kind: &str) -> Option<&'static str> {
        // Shared across languages: parameters read the same wherever the
        // grammar names them so.
        match raw_kind {
            "parameter" | "parameters" | "parameter_declaration" | "typed_parameter" => {
                return Some("parameter");
            }
            _ => {}
        }

        match self {
            #[cfg(feature = "go")]
            TsLanguage::Go => match raw_kind {
                "function_declaration" => Some("function"),
                "method_declaration" => Some("method"),
                "type_declaration" | "type_spec" | "struct_type" | "interface_type" => Some("type"),
                "field_declaration" => Some("field"),
                "const_declaration" | "const_spec" => Some("constant"),
                "var_declaration" | "var_spec" | "short_var_declaration" => Some("variable"),
                "import_declaration" | "import_spec" => Some("import"),
                "source_file" => Some("module"),
                _ => None,
            },
            #[cfg(feature = "rust")]
            TsLanguage::Rust => match raw_kind {
                "function_item" => Some("function"),
                // Rust methods are function_items inside an impl block; the
                // grammar doesn't distinguish at the node level, so a bare
                // function_item is "function". impl-context promotion to
                // "method" is a follow-up (needs parent context).
                "struct_item" | "enum_item" | "trait_item" | "type_item" | "union_item" => {
                    Some("type")
                }
                "field_declaration" => Some("field"),
                "const_item" | "static_item" => Some("constant"),
                "let_declaration" => Some("variable"),
                "use_declaration" => Some("import"),
                "mod_item" | "source_file" => Some("module"),
                _ => None,
            },
            #[cfg(feature = "python")]
            TsLanguage::Python => match raw_kind {
                "function_definition" => Some("function"),
                "class_definition" => Some("type"),
                "import_statement" | "import_from_statement" => Some("import"),
                "module" => Some("module"),
                _ => None,
            },
            #[cfg(feature = "javascript")]
            TsLanguage::JavaScript => match raw_kind {
                "function_declaration" | "function_expression" | "arrow_function" => {
                    Some("function")
                }
                "method_definition" => Some("method"),
                "class_declaration" | "class" => Some("type"),
                "import_statement" => Some("import"),
                "program" => Some("module"),
                _ => None,
            },
            #[cfg(feature = "typescript")]
            // TypeScript extends JavaScript with type-level constructs
            // (`interface_declaration`, `type_alias_declaration`,
            // `enum_declaration`, `abstract_class_declaration`); the
            // shared JS kinds collapse the same way (function decls,
            // method_definition, class_declaration, import_statement).
            TsLanguage::TypeScript => match raw_kind {
                "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "function_signature" => Some("function"),
                "method_definition" | "method_signature" => Some("method"),
                "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration" => Some("type"),
                "import_statement" => Some("import"),
                "program" => Some("module"),
                _ => None,
            },
            // ── Tier 3 query-native languages (bead ley-line-open-5e21c2):
            // κ covers exactly the def kinds queries/<lang>/tags.scm
            // anchors at (mache's dead_code / god_file rules filter on
            // symbol-scope κ), plus the import/module containers. ──
            #[cfg(feature = "java")]
            TsLanguage::Java => match raw_kind {
                "method_declaration" => Some("method"),
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration" => Some("type"),
                "import_declaration" => Some("import"),
                "program" => Some("module"),
                _ => None,
            },
            #[cfg(feature = "c")]
            TsLanguage::C => match raw_kind {
                // Defs anchor at function_declarator (covers definitions
                // AND prototypes); function_definition maps too so the
                // symbols path collapses the enclosing node the same way.
                "function_definition" | "function_declarator" => Some("function"),
                "struct_specifier" | "union_specifier" | "enum_specifier" | "type_definition" => {
                    Some("type")
                }
                "field_declaration" => Some("field"),
                "preproc_include" => Some("import"),
                "translation_unit" => Some("module"),
                _ => None,
            },
            #[cfg(feature = "cpp")]
            // The C++ grammar is a superset of C's; the C kinds collapse
            // identically. In-class methods keep κ "function" (anchor is
            // the same function_declarator kind) — impl-context promotion
            // to "method" is the same documented follow-up as Rust's.
            TsLanguage::Cpp => match raw_kind {
                "function_definition" | "function_declarator" => Some("function"),
                "struct_specifier" | "union_specifier" | "enum_specifier" | "type_definition"
                | "class_specifier" => Some("type"),
                "field_declaration" => Some("field"),
                "preproc_include" => Some("import"),
                "namespace_definition" => Some("module"),
                "translation_unit" => Some("module"),
                _ => None,
            },
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// κ — control-flow node kind collapse for the CFG builder.
    ///
    /// Sibling of `canonical_kind` for the CFG domain (bead
    /// `ley-line-open-46aef2`, decade `dataflow-substrate` T1.b1).
    /// Maps a raw tree-sitter node kind that participates in
    /// intra-procedural control flow to a canonical, language-agnostic
    /// name from `CFG_CANONICAL_KINDS`. Returns `None` for kinds that
    /// carry no control-flow semantics — the caller (T1.b3 CFG builder)
    /// skips those.
    ///
    /// Coverage in this slice is Go + Rust only, matching the T1.b3
    /// scope. Python + JS/TS are gated on `ley-line-open-e76959`
    /// (producer/consumer contract test discipline) so we don't build
    /// higher-order analysis on shaky extraction — see T1.b5.
    ///
    /// The returned kind names identify a raw grammar node's CF role
    /// AT THE AST LEVEL. Synthetic kinds (`loop_back`, `call`, `throw`,
    /// `try_enter`, `try_exit`, `case`) are emitted by the CFG builder
    /// from lower-level structure; no raw kind lands them here directly.
    /// Consumers wanting "all kinds the builder may emit" read
    /// `CFG_CANONICAL_KINDS`, not the codomain of this function.
    pub fn canonical_cfg_kind(self, raw_kind: &str) -> Option<&'static str> {
        match self {
            #[cfg(feature = "go")]
            TsLanguage::Go => match raw_kind {
                "if_statement" => Some("branch"),
                "for_statement" => Some("loop_head"),
                // Go has three switch-shaped nodes: `switch_statement`
                // (expression switch), `type_switch_statement`
                // (type switch), `select_statement` (channel select).
                // All three canonicalize as `switch` — the builder
                // decomposes the cases inside.
                "switch_statement" | "type_switch_statement" | "select_statement" => Some("switch"),
                // Case-clause vocabulary: `expression_case` +
                // `default_case` for expression/type switches;
                // `communication_case` for `select`. Type switches
                // don't have their own named case kind — they share
                // `expression_case` with expression switches.
                "expression_case" | "default_case" | "communication_case" => Some("case"),
                "return_statement" => Some("return"),
                // `defer f()` schedules a call at function-return time;
                // `go f()` starts a goroutine. Both are call-shaped
                // control-flow nodes from the CFG's perspective — the
                // builder emits a `call` block that the intra-procedural
                // walk doesn't descend into, matching how normal
                // `call_expression` sites are handled.
                "defer_statement" | "go_statement" => Some("call"),
                _ => None,
            },
            #[cfg(feature = "rust")]
            TsLanguage::Rust => match raw_kind {
                "if_expression" => Some("branch"),
                // All three loop shapes canonicalize as loop_head:
                // - `while_expression` (condition-driven)
                // - `for_expression` (iterator-driven)
                // - `loop_expression` (unconditional)
                "while_expression" | "for_expression" | "loop_expression" => Some("loop_head"),
                "match_expression" => Some("switch"),
                "match_arm" => Some("case"),
                "return_expression" => Some("return"),
                _ => None,
            },
            #[allow(unreachable_patterns)]
            _ => None,
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
            #[cfg(feature = "javascript")]
            "javascript" | "js" | "jsx" | "mjs" | "cjs" => Ok(TsLanguage::JavaScript),
            #[cfg(feature = "typescript")]
            "typescript" | "ts" | "tsx" => Ok(TsLanguage::TypeScript),
            #[cfg(feature = "sql")]
            "sql" => Ok(TsLanguage::Sql),
            #[cfg(feature = "bash")]
            "bash" | "sh" | "shell" => Ok(TsLanguage::Bash),
            #[cfg(feature = "java")]
            "java" => Ok(TsLanguage::Java),
            #[cfg(feature = "c")]
            "c" | "h" => Ok(TsLanguage::C),
            #[cfg(feature = "cpp")]
            "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Ok(TsLanguage::Cpp),
            #[cfg(feature = "toml")]
            "toml" => Ok(TsLanguage::Toml),
            #[cfg(feature = "dockerfile")]
            "dockerfile" | "containerfile" | "docker" => Ok(TsLanguage::Dockerfile),
            #[cfg(feature = "ruby")]
            "ruby" | "rb" => Ok(TsLanguage::Ruby),
            #[cfg(feature = "php")]
            "php" => Ok(TsLanguage::Php),
            #[cfg(feature = "kotlin")]
            "kotlin" | "kt" | "kts" => Ok(TsLanguage::Kotlin),
            #[cfg(feature = "swift")]
            "swift" => Ok(TsLanguage::Swift),
            #[cfg(feature = "scala")]
            "scala" | "sc" => Ok(TsLanguage::Scala),
            #[cfg(feature = "csharp")]
            "csharp" | "c#" | "cs" | "c-sharp" | "c_sharp" => Ok(TsLanguage::CSharp),
            #[cfg(feature = "css")]
            "css" => Ok(TsLanguage::Css),
            #[cfg(feature = "groovy")]
            "groovy" => Ok(TsLanguage::Groovy),
            #[cfg(feature = "lua")]
            "lua" => Ok(TsLanguage::Lua),
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
            // Extensionless container build files (bead
            // ley-line-open-46ae48). `Dockerfile` is mache's sentinel
            // for its dockerfile registry row; `Containerfile` is the
            // OCI spelling — one grammar covers both.
            #[cfg(feature = "dockerfile")]
            "Dockerfile" | "Containerfile" => Some(TsLanguage::Dockerfile),
            // `Jenkinsfile` is a Groovy DSL — mache's sentinel for its
            // groovy registry row.
            #[cfg(feature = "groovy")]
            "Jenkinsfile" => Some(TsLanguage::Groovy),
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
            #[cfg(feature = "javascript")]
            "js" | "mjs" | "cjs" | "jsx" => Some(TsLanguage::JavaScript),
            // TSX grammar handles both .ts and .tsx uniformly (see
            // `ts_language`); mts/cts are ESM/CJS variants that carry
            // TypeScript syntax and parse identically.
            #[cfg(feature = "typescript")]
            "ts" | "tsx" | "mts" | "cts" => Some(TsLanguage::TypeScript),
            #[cfg(feature = "sql")]
            "sql" => Some(TsLanguage::Sql),
            #[cfg(feature = "bash")]
            "sh" | "bash" => Some(TsLanguage::Bash),
            #[cfg(feature = "java")]
            "java" => Some(TsLanguage::Java),
            // `.h` maps to C, mirroring mache's registry row; C++
            // headers spelled .hpp/.hxx/.hh land on Cpp below.
            #[cfg(feature = "c")]
            "c" | "h" => Some(TsLanguage::C),
            #[cfg(feature = "cpp")]
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Some(TsLanguage::Cpp),
            #[cfg(feature = "toml")]
            "toml" => Some(TsLanguage::Toml),
            // Extension spelling (app.dockerfile); the extensionless
            // `Dockerfile` / `Containerfile` names go via `from_filename`.
            #[cfg(feature = "dockerfile")]
            "dockerfile" => Some(TsLanguage::Dockerfile),
            #[cfg(feature = "ruby")]
            "rb" => Some(TsLanguage::Ruby),
            #[cfg(feature = "php")]
            "php" => Some(TsLanguage::Php),
            #[cfg(feature = "kotlin")]
            "kt" | "kts" => Some(TsLanguage::Kotlin),
            #[cfg(feature = "swift")]
            "swift" => Some(TsLanguage::Swift),
            #[cfg(feature = "scala")]
            "scala" | "sc" => Some(TsLanguage::Scala),
            #[cfg(feature = "csharp")]
            "cs" => Some(TsLanguage::CSharp),
            #[cfg(feature = "css")]
            "css" => Some(TsLanguage::Css),
            #[cfg(feature = "groovy")]
            "groovy" => Some(TsLanguage::Groovy),
            #[cfg(feature = "lua")]
            "lua" => Some(TsLanguage::Lua),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_kind_collapses_function_across_languages() {
        // The load-bearing κ property: three different grammar kinds for
        // "a function" collapse to the one canonical base kind.
        #[cfg(feature = "go")]
        assert_eq!(
            TsLanguage::Go.canonical_kind("function_declaration"),
            Some("function")
        );
        #[cfg(feature = "rust")]
        assert_eq!(
            TsLanguage::Rust.canonical_kind("function_item"),
            Some("function")
        );
        #[cfg(feature = "python")]
        assert_eq!(
            TsLanguage::Python.canonical_kind("function_definition"),
            Some("function")
        );
    }

    #[test]
    fn canonical_kind_maps_other_base_kinds() {
        #[cfg(feature = "go")]
        {
            assert_eq!(
                TsLanguage::Go.canonical_kind("method_declaration"),
                Some("method")
            );
            assert_eq!(
                TsLanguage::Go.canonical_kind("import_declaration"),
                Some("import")
            );
            assert_eq!(
                TsLanguage::Go.canonical_kind("const_declaration"),
                Some("constant")
            );
        }
        #[cfg(feature = "rust")]
        assert_eq!(TsLanguage::Rust.canonical_kind("struct_item"), Some("type"));
    }

    #[test]
    fn kappa_cfg_closed_set_is_stable() {
        // Bead ley-line-open-46aef2. `CFG_CANONICAL_KINDS` is a
        // documented closed set — its contents (and their order,
        // since consumers may index) are part of the CFG contract.
        // Pin every entry so a future refactor can't silently drop
        // one or reorder them.
        assert_eq!(
            CFG_CANONICAL_KINDS,
            [
                "branch",
                "loop_head",
                "loop_back",
                "call",
                "return",
                "throw",
                "try_enter",
                "try_exit",
                "switch",
                "case",
            ],
        );
    }

    #[cfg(feature = "go")]
    #[test]
    fn kappa_cfg_go_maps_control_flow_kinds() {
        // Bead ley-line-open-46aef2. Every Go control-flow raw kind
        // the CFG builder (T1.b3) walks past must land on a canonical
        // CF kind before the builder ever sees it — otherwise T1.b3
        // has to special-case grammar names, breaking κ discipline.
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("if_statement"),
            Some("branch"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("for_statement"),
            Some("loop_head"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("switch_statement"),
            Some("switch"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("type_switch_statement"),
            Some("switch"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("select_statement"),
            Some("switch"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("expression_case"),
            Some("case"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("default_case"),
            Some("case"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("communication_case"),
            Some("case"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("return_statement"),
            Some("return"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("defer_statement"),
            Some("call"),
        );
        assert_eq!(
            TsLanguage::Go.canonical_cfg_kind("go_statement"),
            Some("call"),
        );
    }

    #[cfg(feature = "rust")]
    #[test]
    fn kappa_cfg_rust_maps_control_flow_kinds() {
        // Bead ley-line-open-46aef2. Same discipline as Go — Rust's
        // three loop shapes (`while`, `for`, `loop`) all canonicalize
        // to `loop_head` so T1.b3 doesn't have to distinguish them at
        // the raw grammar level.
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("if_expression"),
            Some("branch"),
        );
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("while_expression"),
            Some("loop_head"),
        );
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("for_expression"),
            Some("loop_head"),
        );
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("loop_expression"),
            Some("loop_head"),
        );
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("match_expression"),
            Some("switch"),
        );
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("match_arm"),
            Some("case"),
        );
        assert_eq!(
            TsLanguage::Rust.canonical_cfg_kind("return_expression"),
            Some("return"),
        );
    }

    #[test]
    fn kappa_cfg_unmapped_returns_none() {
        // Bead ley-line-open-46aef2. Non-CF grammar nodes MUST return
        // None so T1.b3's CFG builder walks past them without emitting
        // a spurious basic block. `identifier`, `block`, and function
        // bodies are the load-bearing negative cases.
        #[cfg(feature = "go")]
        {
            assert_eq!(TsLanguage::Go.canonical_cfg_kind("identifier"), None);
            assert_eq!(TsLanguage::Go.canonical_cfg_kind("block"), None);
            assert_eq!(
                TsLanguage::Go.canonical_cfg_kind("function_declaration"),
                None,
            );
        }
        #[cfg(feature = "rust")]
        {
            assert_eq!(TsLanguage::Rust.canonical_cfg_kind("identifier"), None);
            assert_eq!(TsLanguage::Rust.canonical_cfg_kind("block"), None);
            assert_eq!(TsLanguage::Rust.canonical_cfg_kind("function_item"), None);
        }
    }

    #[test]
    fn kappa_cfg_results_are_members_of_the_closed_set() {
        // Bead ley-line-open-46aef2. Load-bearing invariant:
        // canonical_cfg_kind MUST NEVER return a name outside
        // CFG_CANONICAL_KINDS. If a match arm ever returns e.g.
        // Some("loopHead") (typo) or Some("if") (wrong-domain),
        // ADR-0027 §6 discipline breaks. Exercise the Go + Rust
        // arms across every raw kind we map and pin each result
        // as a member.
        #[cfg(feature = "go")]
        {
            let go_kinds = [
                "if_statement",
                "for_statement",
                "switch_statement",
                "type_switch_statement",
                "select_statement",
                "expression_case",
                "default_case",
                "communication_case",
                "return_statement",
                "defer_statement",
                "go_statement",
            ];
            for raw in go_kinds {
                let canonical = TsLanguage::Go
                    .canonical_cfg_kind(raw)
                    .unwrap_or_else(|| panic!("Go: {raw} must map"));
                assert!(
                    CFG_CANONICAL_KINDS.contains(&canonical),
                    "Go: {raw} → {canonical:?} not in CFG_CANONICAL_KINDS",
                );
            }
        }
        #[cfg(feature = "rust")]
        {
            let rust_kinds = [
                "if_expression",
                "while_expression",
                "for_expression",
                "loop_expression",
                "match_expression",
                "match_arm",
                "return_expression",
            ];
            for raw in rust_kinds {
                let canonical = TsLanguage::Rust
                    .canonical_cfg_kind(raw)
                    .unwrap_or_else(|| panic!("Rust: {raw} must map"));
                assert!(
                    CFG_CANONICAL_KINDS.contains(&canonical),
                    "Rust: {raw} → {canonical:?} not in CFG_CANONICAL_KINDS",
                );
            }
        }
    }

    #[test]
    fn canonical_kind_returns_none_for_unmapped_open_world() {
        // Unmapped kinds fall through to None so the caller keeps the raw
        // kind — the open-world escape hatch, not a panic or a lossy map.
        #[cfg(feature = "go")]
        {
            assert_eq!(TsLanguage::Go.canonical_kind("identifier"), None);
            assert_eq!(TsLanguage::Go.canonical_kind("block"), None);
            assert_eq!(TsLanguage::Go.canonical_kind("not_a_real_kind"), None);
        }
    }

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

    /// Tier 1+2 grammar bulk (bead ley-line-open-46ae48): pin the
    /// extension alias sets for the 16 languages registered at mache's
    /// CGO removal. Extension sets mirror mache's `internal/lang`
    /// Registry rows — dropping one silently skips files at parse time.
    #[test]
    fn from_extension_pins_tier12_alias_sets() {
        #[cfg(feature = "sql")]
        assert_eq!(TsLanguage::from_extension("sql"), Some(TsLanguage::Sql));
        #[cfg(feature = "bash")]
        {
            assert_eq!(TsLanguage::from_extension("sh"), Some(TsLanguage::Bash));
            assert_eq!(TsLanguage::from_extension("bash"), Some(TsLanguage::Bash));
        }
        #[cfg(feature = "java")]
        assert_eq!(TsLanguage::from_extension("java"), Some(TsLanguage::Java));
        #[cfg(feature = "c")]
        {
            assert_eq!(TsLanguage::from_extension("c"), Some(TsLanguage::C));
            // .h maps to C, matching mache's registry row. C++ headers
            // spelled .hpp/.hxx/.hh land on Cpp below.
            assert_eq!(TsLanguage::from_extension("h"), Some(TsLanguage::C));
        }
        #[cfg(feature = "cpp")]
        {
            for ext in ["cpp", "cc", "cxx", "hpp", "hxx", "hh"] {
                assert_eq!(
                    TsLanguage::from_extension(ext),
                    Some(TsLanguage::Cpp),
                    "extension {ext:?} must resolve to Cpp"
                );
            }
        }
        #[cfg(feature = "toml")]
        assert_eq!(TsLanguage::from_extension("toml"), Some(TsLanguage::Toml));
        #[cfg(feature = "dockerfile")]
        assert_eq!(
            TsLanguage::from_extension("dockerfile"),
            Some(TsLanguage::Dockerfile)
        );
        #[cfg(feature = "ruby")]
        assert_eq!(TsLanguage::from_extension("rb"), Some(TsLanguage::Ruby));
        #[cfg(feature = "php")]
        assert_eq!(TsLanguage::from_extension("php"), Some(TsLanguage::Php));
        #[cfg(feature = "kotlin")]
        {
            assert_eq!(TsLanguage::from_extension("kt"), Some(TsLanguage::Kotlin));
            assert_eq!(TsLanguage::from_extension("kts"), Some(TsLanguage::Kotlin));
        }
        #[cfg(feature = "swift")]
        assert_eq!(TsLanguage::from_extension("swift"), Some(TsLanguage::Swift));
        #[cfg(feature = "scala")]
        {
            assert_eq!(TsLanguage::from_extension("scala"), Some(TsLanguage::Scala));
            assert_eq!(TsLanguage::from_extension("sc"), Some(TsLanguage::Scala));
        }
        #[cfg(feature = "csharp")]
        assert_eq!(TsLanguage::from_extension("cs"), Some(TsLanguage::CSharp));
        #[cfg(feature = "css")]
        assert_eq!(TsLanguage::from_extension("css"), Some(TsLanguage::Css));
        #[cfg(feature = "groovy")]
        assert_eq!(
            TsLanguage::from_extension("groovy"),
            Some(TsLanguage::Groovy)
        );
        #[cfg(feature = "lua")]
        assert_eq!(TsLanguage::from_extension("lua"), Some(TsLanguage::Lua));

        // Case-insensitivity holds for the new sets too.
        #[cfg(feature = "sql")]
        assert_eq!(TsLanguage::from_extension("SQL"), Some(TsLanguage::Sql));
        #[cfg(feature = "java")]
        assert_eq!(TsLanguage::from_extension("JAVA"), Some(TsLanguage::Java));
    }

    /// Tier 1+2 grammar bulk (bead ley-line-open-46ae48): pin the
    /// `from_name` alias sets. Consumers (mache, daemon callers) pass
    /// the language tag explicitly; every documented spelling must
    /// round-trip to the one canonical variant.
    #[test]
    fn from_name_pins_tier12_alias_sets() {
        #[allow(unused)]
        fn pin(spellings: &[&str], want: TsLanguage) {
            for s in spellings {
                let got =
                    TsLanguage::from_name(s).unwrap_or_else(|e| panic!("from_name({s:?}): {e}"));
                assert_eq!(got, want, "spelling {s:?} must resolve to {want:?}");
            }
        }
        #[cfg(feature = "sql")]
        pin(&["sql", "SQL"], TsLanguage::Sql);
        #[cfg(feature = "bash")]
        pin(&["bash", "sh", "shell", "Bash"], TsLanguage::Bash);
        #[cfg(feature = "java")]
        pin(&["java", "Java"], TsLanguage::Java);
        #[cfg(feature = "c")]
        pin(&["c", "C", "h"], TsLanguage::C);
        #[cfg(feature = "cpp")]
        pin(
            &["cpp", "c++", "cc", "cxx", "hpp", "hxx", "hh", "CPP"],
            TsLanguage::Cpp,
        );
        #[cfg(feature = "toml")]
        pin(&["toml", "TOML"], TsLanguage::Toml);
        #[cfg(feature = "dockerfile")]
        pin(
            &["dockerfile", "containerfile", "docker", "Dockerfile"],
            TsLanguage::Dockerfile,
        );
        #[cfg(feature = "ruby")]
        pin(&["ruby", "rb", "Ruby"], TsLanguage::Ruby);
        #[cfg(feature = "php")]
        pin(&["php", "PHP"], TsLanguage::Php);
        #[cfg(feature = "kotlin")]
        pin(&["kotlin", "kt", "kts", "Kotlin"], TsLanguage::Kotlin);
        #[cfg(feature = "swift")]
        pin(&["swift", "Swift"], TsLanguage::Swift);
        #[cfg(feature = "scala")]
        pin(&["scala", "sc", "Scala"], TsLanguage::Scala);
        #[cfg(feature = "csharp")]
        pin(
            &["csharp", "c#", "cs", "c-sharp", "c_sharp", "CSharp"],
            TsLanguage::CSharp,
        );
        #[cfg(feature = "css")]
        pin(&["css", "CSS"], TsLanguage::Css);
        #[cfg(feature = "groovy")]
        pin(&["groovy", "Groovy"], TsLanguage::Groovy);
        #[cfg(feature = "lua")]
        pin(&["lua", "Lua"], TsLanguage::Lua);
    }

    /// Extensionless well-known filenames (bead ley-line-open-46ae48):
    /// `Dockerfile` / `Containerfile` (mache sentinel for the dockerfile
    /// row) and `Jenkinsfile` (mache sentinel for groovy) resolve via
    /// `from_filename`, since neither carries an extension.
    #[test]
    fn from_filename_pins_tier12_well_known_names() {
        #[cfg(feature = "dockerfile")]
        {
            assert_eq!(
                TsLanguage::from_filename("Dockerfile"),
                Some(TsLanguage::Dockerfile)
            );
            assert_eq!(
                TsLanguage::from_filename("Containerfile"),
                Some(TsLanguage::Dockerfile)
            );
        }
        #[cfg(feature = "groovy")]
        assert_eq!(
            TsLanguage::from_filename("Jenkinsfile"),
            Some(TsLanguage::Groovy)
        );
    }

    /// Tier 1+2 languages carry no def/ref algebra yet — `canonical_kind`
    /// returns `None` for every raw kind (the open-world escape), so the
    /// caller stores raw kinds untouched. For the config/data languages
    /// (toml, dockerfile, css) this is BY DESIGN — parse/validate only.
    /// For the remaining code languages (sql, bash, ruby, php, kotlin,
    /// swift, scala, csharp, groovy, lua) Tier 3 lands as .scm query
    /// data over the generic engine — separate work, never match arms
    /// here. java/c/cpp graduated to Tier 3 (bead ley-line-open-5e21c2)
    /// and are pinned positively in
    /// `tier3_query_native_languages_map_emitted_def_kinds`.
    #[test]
    fn tier12_languages_have_no_canonical_kind_mapping() {
        #[allow(unused)]
        let probe = |lang: TsLanguage| {
            for raw in ["function_definition", "class_declaration", "block", "table"] {
                assert_eq!(
                    lang.canonical_kind(raw),
                    None,
                    "{lang:?}: Tier 1+2 languages must not map {raw:?} (no extractor algebra)"
                );
            }
            assert_eq!(lang.canonical_cfg_kind("if_statement"), None);
        };
        #[cfg(feature = "sql")]
        probe(TsLanguage::Sql);
        #[cfg(feature = "toml")]
        probe(TsLanguage::Toml);
        #[cfg(feature = "dockerfile")]
        probe(TsLanguage::Dockerfile);
        #[cfg(feature = "css")]
        probe(TsLanguage::Css);
        #[cfg(feature = "lua")]
        probe(TsLanguage::Lua);
    }

    /// Tier 3 query-native languages (bead ley-line-open-5e21c2): κ
    /// covers exactly the def kinds `queries/<lang>/tags.scm` anchors
    /// at — the engine derives a def's `canonical_kind` from its anchor
    /// node's raw kind, and mache's dead_code / god_file rules filter
    /// on symbol-scope κ. An anchor kind mapping to `None` would write
    /// NULL-kind def rows those rules silently skip.
    #[test]
    fn tier3_query_native_languages_map_emitted_def_kinds() {
        #[cfg(feature = "java")]
        {
            assert_eq!(
                TsLanguage::Java.canonical_kind("method_declaration"),
                Some("method")
            );
            for raw in [
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "record_declaration",
            ] {
                assert_eq!(
                    TsLanguage::Java.canonical_kind(raw),
                    Some("type"),
                    "Java: {raw} must collapse to type"
                );
            }
            assert_eq!(
                TsLanguage::Java.canonical_kind("import_declaration"),
                Some("import")
            );
            assert_eq!(TsLanguage::Java.canonical_kind("program"), Some("module"));
        }
        #[cfg(feature = "c")]
        {
            assert_eq!(
                TsLanguage::C.canonical_kind("function_declarator"),
                Some("function")
            );
            assert_eq!(
                TsLanguage::C.canonical_kind("function_definition"),
                Some("function")
            );
            for raw in [
                "struct_specifier",
                "union_specifier",
                "enum_specifier",
                "type_definition",
            ] {
                assert_eq!(
                    TsLanguage::C.canonical_kind(raw),
                    Some("type"),
                    "C: {raw} must collapse to type"
                );
            }
            assert_eq!(
                TsLanguage::C.canonical_kind("preproc_include"),
                Some("import")
            );
            assert_eq!(
                TsLanguage::C.canonical_kind("translation_unit"),
                Some("module")
            );
        }
        #[cfg(feature = "cpp")]
        {
            assert_eq!(
                TsLanguage::Cpp.canonical_kind("function_declarator"),
                Some("function")
            );
            assert_eq!(
                TsLanguage::Cpp.canonical_kind("class_specifier"),
                Some("type")
            );
            assert_eq!(
                TsLanguage::Cpp.canonical_kind("struct_specifier"),
                Some("type")
            );
            assert_eq!(
                TsLanguage::Cpp.canonical_kind("namespace_definition"),
                Some("module")
            );
            assert_eq!(
                TsLanguage::Cpp.canonical_kind("preproc_include"),
                Some("import")
            );
        }
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
