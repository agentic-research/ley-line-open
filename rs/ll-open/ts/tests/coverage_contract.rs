//! Per-language extraction coverage contracts (bead `ley-line-open-63cb83`,
//! prompted by `651909` / `06aebb` / `ea1e42` all being discovered by
//! DOWNSTREAM consumers in one day).
//!
//! # The problem this closes
//!
//! LLO ships partial extractors — legitimately; SQL's tags.scm header
//! proves deliberate partiality works — but the partiality was
//! UNDECLARED. Downstream rules (`dead_code`,
//! `drift_doc_dead_symbol_reference`) treat absence of a fact as
//! evidence, and absence is only evidence under a completeness claim.
//! Every undeclared gap therefore turns into a false claim downstream:
//! Go consts "look dead" (651909), doc citations of them false-positive.
//! Three consumers found three gaps for us in one day. The knowledge
//! was flowing backward.
//!
//! # The contract
//!
//! Every language listed here declares, against a fixture that
//! exercises its declarable constructs:
//!
//! - **`expect_def`** — constructs that MUST emit a def with this
//!   token. A regression (a query edit dropping a pattern) fails here.
//! - **`ledgered_absent`** — constructs that are KNOWN not to emit,
//!   each with the bead that owns the gap. These assert the token is
//!   genuinely absent from the def set — so when the gap is fixed, the
//!   stale ledger entry FAILS the build and must be promoted to
//!   `expect_def`. The ledger cannot rot in either direction.
//!
//! Languages not yet under contract sit in `NOT_YET_COVERED` —
//! shrink-only, exact-match, so the "unconsidered" bucket is a visible
//! ratchet instead of silence. Adding a language to the workspace
//! without deciding its contract fails this test.
//!
//! The fixture-driven design measures the REAL extractor end to end
//! (grammar → query/imperative arm → `ExtractedRef`), not a reading of
//! the `.scm` — the same "assert the mechanism fired" rule as
//! everything else in this repo.

use leyline_ts::languages::TsLanguage;
use leyline_ts::refs::{ExtractedRef, extract_refs};

struct Contract {
    /// Language name as `TsLanguage::from_name` resolves it. A NAME,
    /// not the enum variant, so this file carries no `#[cfg(feature)]`
    /// gates (the repo's cfg_feature_in_tests smell — a cfg-gated test
    /// silently skips when the feature is off, the exact silent-skip
    /// class this harness exists to fight). Contracts for languages
    /// not compiled into the current build are skipped at RUNTIME with
    /// an eprintln, visibly.
    lang_name: &'static str,
    fixture: &'static str,
    /// (construct label, token that must appear as a def)
    expect_def: &'static [(&'static str, &'static str)],
    /// (construct label, token that must NOT appear as a def, owning bead)
    ledgered_absent: &'static [(&'static str, &'static str, &'static str)],
}

/// Languages compiled into this workspace that do not yet declare a
/// coverage contract. Shrink-only: removing one means writing its
/// contract below; ADDING one is a deliberate act that must name why
/// the new language ships uncontracted.
const NOT_YET_COVERED: &[&str] = &[
    "rust",
    "python",
    "javascript",
    "java",
    "c",
    "cpp",
    "sql",
    "bash",
];

fn contracts() -> Vec<Contract> {
    let mut out: Vec<Contract> = Vec::new();
    out.push(Contract {
        lang_name: "go",
        fixture: r#"package main

import "fmt"

const TopicFoo = "foo"

var packageVar = 7

type Widget struct{ id int }

func (w *Widget) Render() string { return "w" }

func Publish(topic string) { fmt.Println(topic) }

func caller() {
    Publish(TopicFoo)
}
"#,
        expect_def: &[
            ("function declaration", "Publish"),
            ("method declaration (bare)", "Render"),
            ("method declaration (qualified dual-emit)", "Widget.Render"),
            ("type declaration", "Widget"),
        ],
        ledgered_absent: &[
            ("package-level const", "TopicFoo", "ley-line-open-651909"),
            ("package-level var", "packageVar", "ley-line-open-651909"),
        ],
    });
    out.push(Contract {
        // HCL's contracted algebra is ref-only: typed cross-language
        // locators for Terraform variables and module sources. The
        // dedicated test below pins the exact ref set and negative cases.
        lang_name: "hcl",
        fixture: r#"variable "DATABASE_URL" {
  default = "postgres://localhost/app"
}

module "app" {
  version = "1.0.0"
  source  = "./modules/app"
}

module "dynamic" {
  source = local.module_source
}

resource "aws_instance" "web" {
  source = "resource-source-must-not-emit"
}
"#,
        expect_def: &[],
        ledgered_absent: &[],
    });
    out.push(Contract {
        lang_name: "typescript",
        fixture: r#"import { z } from "zod";

export function plainFn(): void {}

export const arrowFn = () => 1;

export const schema = z.object({ id: z.string() });

export const LIMIT = 42;

export class Widget {
    render(): string { return "w"; }
}

export interface Shape { id: string }

export type Alias = Shape;

export enum Mode { A, B }
"#,
        expect_def: &[
            ("function declaration", "plainFn"),
            ("const arrow-function binding", "arrowFn"),
            ("class declaration", "Widget"),
            ("method definition", "render"),
            ("interface declaration", "Shape"),
            ("type alias", "Alias"),
            ("enum declaration", "Mode"),
        ],
        ledgered_absent: &[
            (
                "const non-function binding (object/schema)",
                "schema",
                "ley-line-open-06aebb (measured immaterial on 0day; reopen if a consumer hits it)",
            ),
            (
                "const non-function binding (scalar)",
                "LIMIT",
                "ley-line-open-06aebb (measured immaterial on 0day; reopen if a consumer hits it)",
            ),
        ],
    });
    out.push(Contract {
        // Reached only via injection from the Markdown block grammar
        // (ea1e42); contracted here directly against INLINE_LANGUAGE
        // because the extractor arm is per-node and pure.
        lang_name: "markdown-inline",
        fixture: "Calls `PartitionSpec::address` and plain *emphasis* and [a link](https://x).\n",
        expect_def: &[],
        ledgered_absent: &[],
    });
    out
}

/// Parse `fixture` under `lang` and run the real extractor over every
/// named node, collecting def tokens and ref tokens.
fn extract_all(lang: TsLanguage, fixture: &str) -> (Vec<String>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.ts_language())
        .expect("grammar loads");
    let tree = parser.parse(fixture, None).expect("fixture parses");

    let mut defs = Vec::new();
    let mut refs = Vec::new();
    fn walk(
        node: tree_sitter::Node,
        src: &[u8],
        lang: TsLanguage,
        defs: &mut Vec<String>,
        refs: &mut Vec<String>,
    ) {
        for r in extract_refs(&node, src, "id", "src", lang, None) {
            match r {
                ExtractedRef::Def { token, .. } => defs.push(token),
                ExtractedRef::Ref { token, .. } => refs.push(token),
                ExtractedRef::Import { .. } => {}
            }
        }
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                if c.node().is_named() {
                    walk(c.node(), src, lang, defs, refs);
                }
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    walk(
        tree.root_node(),
        fixture.as_bytes(),
        lang,
        &mut defs,
        &mut refs,
    );
    (defs, refs)
}

/// Resolve a contract's language, or skip it VISIBLY when the feature
/// is not compiled into this build. Runtime probing instead of
/// `#[cfg(feature)]` on tests: a cfg-gated test silently vanishes from
/// the run, which is the silent-skip class this harness exists to
/// fight (and the repo's cfg_feature_in_tests smell rule bans).
fn resolve_or_skip(c: &Contract) -> Option<TsLanguage> {
    match TsLanguage::from_name(c.lang_name) {
        Ok(lang) => Some(lang),
        Err(_) => {
            eprintln!(
                "coverage-contract: skipping {} — feature not compiled into this build",
                c.lang_name
            );
            None
        }
    }
}

#[test]
fn every_contracted_language_emits_what_it_declares_and_nothing_it_ledgers() {
    for c in contracts() {
        let Some(lang) = resolve_or_skip(&c) else {
            continue;
        };
        let (defs, _refs) = extract_all(lang, c.fixture);

        for (label, token) in c.expect_def {
            assert!(
                defs.iter().any(|t| t == token),
                "{}: {label} must emit def token {token:?} but the extractor \
                 emitted defs {defs:?}. A query/extractor edit dropped \
                 declared coverage — restore it or move the construct to \
                 the ledger WITH a bead.",
                c.lang_name,
            );
        }

        for (label, token, bead) in c.ledgered_absent {
            assert!(
                !defs.iter().any(|t| t == token),
                "{}: {label} ({token:?}) is ledgered as NOT emitting (owned \
                 by {bead}) but the extractor now emits it. Coverage \
                 improved — promote this entry to expect_def and update \
                 the bead; a stale ledger is the lie this test exists to \
                 prevent.",
                c.lang_name,
            );
        }
    }
}

/// The markdown-inline contract is ref-shaped, not def-shaped: a code
/// span is a doc-symbol REFERENCE (the mache join surface), and prose
/// structure deliberately emits nothing.
#[test]
fn markdown_inline_emits_code_span_refs_and_no_prose_facts() {
    let c = contracts()
        .into_iter()
        .find(|c| c.lang_name == "markdown-inline")
        .expect("markdown-inline contract exists");
    let Some(lang) = resolve_or_skip(&c) else {
        return;
    };
    let (defs, refs) = extract_all(lang, c.fixture);

    assert_eq!(
        refs,
        vec!["PartitionSpec::address".to_string()],
        "exactly the code span, delimiters stripped"
    );
    assert!(
        defs.is_empty(),
        "markdown-inline never emits defs; got {defs:?}"
    );
}

/// HCL's contract is also ref-shaped: variable labels and literal module
/// sources become typed cross-language locators. Dynamic module sources and
/// non-module `source` attributes are not stable locators and stay silent.
#[test]
fn hcl_emits_only_declared_typed_address_refs() {
    let c = contracts()
        .into_iter()
        .find(|c| c.lang_name == "hcl")
        .expect("hcl contract exists");
    let Some(lang) = resolve_or_skip(&c) else {
        return;
    };
    let (defs, mut refs) = extract_all(lang, c.fixture);
    refs.sort();

    assert_eq!(
        refs,
        vec![
            "env:DATABASE_URL".to_string(),
            "mod:./modules/app".to_string(),
        ]
    );
    assert!(defs.is_empty(), "HCL address extraction emits no defs");
}

/// The unconsidered bucket, made visible and ratcheted. Exact match:
/// contracting a language means removing it HERE in the same commit;
/// adding a workspace language means deciding, in this file, whether it
/// gets a contract or a listed exemption.
#[test]
fn uncontracted_languages_are_exactly_the_declared_ratchet_list() {
    let contracted: Vec<&str> = contracts().iter().map(|c| c.lang_name).collect();

    // The languages with an extract_refs dispatch arm — the fact-emitting
    // set, which is what a coverage contract is ABOUT. Structural-only
    // languages (json/yaml/toml/html/...) emit nothing by design and are
    // out of scope. KNOWN HAND-MIRROR: this list must track refs.rs's
    // dispatch match; deriving it mechanically is part of
    // ley-line-open-63cb83. Probed through from_name so only the
    // compiled-in subset is asserted under any feature set.
    let fact_emitting = [
        "go",
        "hcl",
        "rust",
        "python",
        "javascript",
        "typescript",
        "java",
        "c",
        "cpp",
        "sql",
        "bash",
        "markdown-inline",
    ];

    for lang in fact_emitting {
        if TsLanguage::from_name(lang).is_err() {
            continue; // feature not compiled into this build
        }
        let is_contracted = contracted.contains(&lang);
        let is_ratcheted = NOT_YET_COVERED.contains(&lang);
        assert!(
            is_contracted ^ is_ratcheted,
            "{lang}: must be either contracted or explicitly listed in \
             NOT_YET_COVERED (contracted={is_contracted}, \
             ratcheted={is_ratcheted}). Silence is the failure mode this \
             file exists to remove."
        );
    }
}
