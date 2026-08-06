#![cfg(feature = "cdc")]

//! Split out of `cdc_activation_consumer_test.rs` (`ley-line-open-c3d746`
//! routing fix): this file's `include_str!("../../../../README.md")` reaches
//! outside the crate, which fails to resolve in cargo-mutants' scratch build
//! and breaks the WHOLE binary's baseline — the same class of failure
//! `tools/mutants_diff.sh` documents for mcp-descriptor's
//! `schema_conformance`. Kept out of the mutants-routed
//! `cdc_activation_consumer_test` binary so that routing stays possible;
//! this file is intentionally never added to that routing.

#[test]
fn release_docs_pin_the_private_derived_ownership_contract() {
    let readme = include_str!("../../../../README.md");
    let changelog = include_str!("../../../../CHANGELOG.md");
    let normalized_readme = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    // Assert the CLAIM, not its line wrapping. The previous form matched a
    // literal "private derived\nindexes" — a hard-wrap position — which is why
    // the README carried a duplicated, unindented copy of this sentence purely
    // to satisfy it. A gate that forces the prose it guards to be wrong is
    // worse than no gate; these match on whitespace-normalized text so the
    // paragraph can be rewrapped or reworded without weakening the contract.
    for claim in [
        "`content_chunks`, `content_manifest`, and `content_manifest_meta` tables",
        "are private derived indexes",
        "do not bump `leyline-schema`",
    ] {
        assert!(
            normalized_readme.contains(claim),
            "README must state the private-derived ownership contract; missing: {claim}"
        );
    }
    // The authoritative-record claim is the load-bearing half — CDC may never
    // be described as replacing `nodes.record` (ADR-0033 D1).
    assert!(
        normalized_readme.contains("never replace the authoritative record"),
        "README must state that the private indexes never replace the authoritative record"
    );
    let v0102 = changelog
        .split("## [0.10.2]")
        .nth(1)
        .and_then(|rest| rest.split("\n## [").next())
        .expect("v0.10.2 changelog section");
    assert!(
        v0102.contains("still rebuilt the full manifest"),
        "the immutable v0.10.2 history must describe what that release shipped"
    );
    let v0103 = changelog
        .split("## [0.10.3]")
        .nth(1)
        .and_then(|rest| rest.split("\n## [").next())
        .expect("v0.10.3 changelog section");
    assert!(
        v0103.contains("Incremental CDC writes"),
        "the release that wires incremental writes must claim them in its own section"
    );
}
