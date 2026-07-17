//! Toy restriction-addressed cache harness for code-review facts.
//!
//! This module is deliberately narrow. It is not a Rust parser and it does
//! not participate in production cache invalidation. It exists to make the
//! ADR-0030 reframe executable: compare a whole-object hash, an
//! identifier-blind AST-shape hash, and a fact-specific review restriction
//! hash against review facts derived from the same toy AST.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CachePolicy {
    WholeObject,
    AstShape,
    ReviewRestriction,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReviewFactKind {
    UsesUnwrap,
    PublicSignatureChanged,
    CallTargetChanged,
    ImportSurfaceChanged,
    BranchConditionChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyOutcome {
    pub policy: CachePolicy,
    pub would_skip: bool,
    pub false_skip: bool,
    pub saved_recompute: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenarioReport {
    pub changed_facts: BTreeSet<ReviewFactKind>,
    pub outcomes: Vec<PolicyOutcome>,
}

impl ScenarioReport {
    pub fn outcome(&self, policy: CachePolicy) -> Option<&PolicyOutcome> {
        self.outcomes
            .iter()
            .find(|outcome| outcome.policy == policy)
    }

    pub fn as_table_row(&self, label: &str) -> String {
        let whole = self
            .outcome(CachePolicy::WholeObject)
            .expect("whole-object policy present");
        let shape = self
            .outcome(CachePolicy::AstShape)
            .expect("AST-shape policy present");
        let review = self
            .outcome(CachePolicy::ReviewRestriction)
            .expect("review-restriction policy present");
        format!(
            "{label:<18} facts={:?} | whole skip={} false={} | shape skip={} false={} | review skip={} false={}",
            self.changed_facts,
            whole.would_skip,
            whole.false_skip,
            shape.would_skip,
            shape.false_skip,
            review.would_skip,
            review.false_skip
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToyAst {
    shape: Vec<String>,
    review: ReviewSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewSnapshot {
    uses_unwrap: bool,
    public_signatures: BTreeSet<String>,
    call_targets: BTreeSet<String>,
    imports: BTreeSet<String>,
    branch_conditions: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CacheKeys {
    object: [u8; 32],
    ast_shape: [u8; 32],
    review_restriction: [u8; 32],
}

pub fn compare_review_cache(before: &str, after: &str) -> ScenarioReport {
    let before_ast = parse_toy_ast(before);
    let after_ast = parse_toy_ast(after);
    let before_keys = cache_keys(before, &before_ast);
    let after_keys = cache_keys(after, &after_ast);
    let changed_facts = changed_review_facts(&before_ast.review, &after_ast.review);
    let facts_changed = !changed_facts.is_empty();

    let policies = [
        (
            CachePolicy::WholeObject,
            before_keys.object == after_keys.object,
        ),
        (
            CachePolicy::AstShape,
            before_keys.ast_shape == after_keys.ast_shape,
        ),
        (
            CachePolicy::ReviewRestriction,
            before_keys.review_restriction == after_keys.review_restriction,
        ),
    ];
    let outcomes = policies
        .into_iter()
        .map(|(policy, would_skip)| PolicyOutcome {
            policy,
            would_skip,
            false_skip: would_skip && facts_changed,
            saved_recompute: would_skip && !facts_changed,
        })
        .collect();

    ScenarioReport {
        changed_facts,
        outcomes,
    }
}

fn cache_keys(source: &str, ast: &ToyAst) -> CacheKeys {
    CacheKeys {
        object: hash_bytes(source.as_bytes()),
        ast_shape: hash_bytes(ast.shape.join("\n").as_bytes()),
        review_restriction: hash_bytes(ast.review.canonical().as_bytes()),
    }
}

fn parse_toy_ast(source: &str) -> ToyAst {
    let mut shape = Vec::new();
    let mut public_signatures = BTreeSet::new();
    let mut call_targets = BTreeSet::new();
    let mut imports = BTreeSet::new();
    let mut branch_conditions = BTreeSet::new();
    let mut uses_unwrap = false;

    for raw in source.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        shape.push(shape_token(line));

        if line.starts_with("use ") {
            imports.insert(canonical_ws(line.trim_end_matches(';')));
        }
        if line.starts_with("pub fn ") {
            public_signatures.insert(public_signature(line));
            continue;
        }
        if let Some(condition) = branch_condition(line) {
            branch_conditions.insert(condition);
        }
        if line.contains(".unwrap(") || line.contains(".unwrap()") {
            uses_unwrap = true;
        }
        for target in call_targets_in_line(line) {
            call_targets.insert(target);
        }
    }

    ToyAst {
        shape,
        review: ReviewSnapshot {
            uses_unwrap,
            public_signatures,
            call_targets,
            imports,
            branch_conditions,
        },
    }
}

fn changed_review_facts(
    before: &ReviewSnapshot,
    after: &ReviewSnapshot,
) -> BTreeSet<ReviewFactKind> {
    let mut changed = BTreeSet::new();
    if before.uses_unwrap != after.uses_unwrap {
        changed.insert(ReviewFactKind::UsesUnwrap);
    }
    if before.public_signatures != after.public_signatures {
        changed.insert(ReviewFactKind::PublicSignatureChanged);
    }
    if before.call_targets != after.call_targets {
        changed.insert(ReviewFactKind::CallTargetChanged);
    }
    if before.imports != after.imports {
        changed.insert(ReviewFactKind::ImportSurfaceChanged);
    }
    if before.branch_conditions != after.branch_conditions {
        changed.insert(ReviewFactKind::BranchConditionChanged);
    }
    changed
}

impl ReviewSnapshot {
    fn canonical(&self) -> String {
        format!(
            "unwrap={}\npublic={}\nimports={}\ncalls={}\nbranches={}",
            self.uses_unwrap,
            join_set(&self.public_signatures),
            join_set(&self.imports),
            join_set(&self.call_targets),
            join_set(&self.branch_conditions)
        )
    }
}

fn shape_token(line: &str) -> String {
    if line.starts_with("use ") {
        "use".into()
    } else if line.starts_with("pub fn ") {
        "pub_fn".into()
    } else if line.starts_with("fn ") {
        "fn".into()
    } else if line.starts_with("if ") {
        "if".into()
    } else if line.starts_with("} else") || line == "else {" {
        "else".into()
    } else if line == "{" || line == "}" || line == "};" {
        "brace".into()
    } else if line.contains('(') && line.contains(')') {
        "expr_call".into()
    } else {
        "expr".into()
    }
}

fn public_signature(line: &str) -> String {
    let signature = line.split('{').next().unwrap_or(line);
    canonical_ws(signature)
}

fn branch_condition(line: &str) -> Option<String> {
    let rest = line.strip_prefix("if ")?;
    let condition = rest.split('{').next().unwrap_or(rest);
    Some(canonical_ws(condition))
}

fn call_targets_in_line(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] != b'(' {
            idx += 1;
            continue;
        }

        let mut end = idx;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && is_ident_byte(bytes[start - 1]) {
            start -= 1;
        }
        if start < end {
            let ident = &line[start..end];
            if !matches!(ident, "if" | "for" | "while" | "match" | "fn") {
                targets.push(ident.to_string());
            }
        }
        idx += 1;
    }
    targets
}

fn strip_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

fn canonical_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn join_set(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join("|")
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
