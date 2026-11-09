//! One review cache, three key strategies (bead `ley-line-open-754aa1`).
//!
//! The cache is a content-addressed memo: a value is stored under the key
//! its strategy computes and served whenever a later lookup computes the same
//! key. Soundness is the strategy's property, not the cache's: a key must
//! change whenever the value would. The three strategies are the ablation's
//! arms:
//!
//! | arm | key | sound | invalidated by |
//! |---|---|---|---|
//! | [`NoCache`] | none | trivially | everything (never stores) |
//! | [`WholeInput`] | [`whole_input_key`]`(storeRoot, parserId, target)` | yes | any edit anywhere in the input tree |
//! | [`RestrictionKey`] | [`restriction_for_call_target`] | iff the closure is complete | an edit to a row the review reads |
//!
//! A daemon consumer (bead `ley-line-open-69305e`) holds one
//! [`ReviewCache`] per review family and calls [`ReviewCache::get_or_compute`]
//! with the [`RestrictionKey`] of the request.

use std::collections::HashMap;

use leyline_core::Hash;

use crate::restriction::{FactSubstrate, restriction_for_call_target, whole_input_key};

/// Everything a key strategy may read about one review request.
pub struct ReviewInput<'a> {
    /// The fact rows the review runs over.
    pub substrate: &'a FactSubstrate,
    /// The input tree's root (`Head.storeRoot` in the daemon).
    pub store_root: Hash,
    /// The review target: the function's container identity.
    pub target: &'a str,
}

/// How a cache keys a review request. `None` means "do not cache".
pub trait KeyStrategy {
    /// A short name for reports.
    fn name(&self) -> &'static str;
    /// The key of `input`. `rows_touched` counts substrate rows read.
    fn key(&self, input: &ReviewInput<'_>, rows_touched: &mut u64) -> Option<Hash>;
}

/// Never caches: every request recomputes.
pub struct NoCache;

impl KeyStrategy for NoCache {
    fn name(&self) -> &'static str {
        "no-cache"
    }
    fn key(&self, _: &ReviewInput<'_>, _: &mut u64) -> Option<Hash> {
        None
    }
}

/// Keys on the whole input tree and the target.
pub struct WholeInput;

impl KeyStrategy for WholeInput {
    fn name(&self) -> &'static str {
        "whole-input"
    }
    fn key(&self, input: &ReviewInput<'_>, _: &mut u64) -> Option<Hash> {
        Some(whole_input_key(
            input.store_root,
            input.substrate.parser_id(),
            input.target,
        ))
    }
}

/// Keys on the call-target review's input closure.
pub struct RestrictionKey;

impl KeyStrategy for RestrictionKey {
    fn name(&self) -> &'static str {
        "restriction"
    }
    fn key(&self, input: &ReviewInput<'_>, rows_touched: &mut u64) -> Option<Hash> {
        Some(restriction_for_call_target(
            input.substrate,
            input.target,
            rows_touched,
        ))
    }
}

/// A content-addressed memo of review values.
pub struct ReviewCache<V> {
    entries: HashMap<Hash, V>,
}

impl<V> Default for ReviewCache<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> ReviewCache<V> {
    pub fn new() -> Self {
        ReviewCache {
            entries: HashMap::new(),
        }
    }

    /// The value stored under `key`, if any. A `None` key never hits.
    pub fn get(&self, key: Option<&Hash>) -> Option<&V> {
        key.and_then(|k| self.entries.get(k))
    }

    /// Store `value` under `key`. A `None` key stores nothing.
    pub fn insert(&mut self, key: Option<Hash>, value: V) {
        if let Some(k) = key {
            self.entries.insert(k, value);
        }
    }

    /// Number of stored values.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<V: Clone> ReviewCache<V> {
    /// The stored value for `key`, or `compute()` stored under it. Returns
    /// the value and whether it was a hit.
    pub fn get_or_compute(&mut self, key: Option<Hash>, compute: impl FnOnce() -> V) -> (V, bool) {
        if let Some(v) = self.get(key.as_ref()) {
            return (v.clone(), true);
        }
        let v = compute();
        self.insert(key, v.clone());
        (v, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restriction::{DefRow, RefRow};
    use std::collections::BTreeMap;

    fn h(b: u8) -> Hash {
        Hash::from_bytes([b; 32])
    }

    fn substrate(callee: &str) -> FactSubstrate {
        FactSubstrate::from_rows(
            vec![DefRow {
                token: callee.into(),
                source_id: "m.rs".into(),
                kind: "function".into(),
            }],
            vec![RefRow {
                token: callee.into(),
                qualifier: None,
                container: Some("F".into()),
                source_id: "main.rs".into(),
            }],
            vec![],
            BTreeMap::new(),
            h(1),
        )
    }

    fn input<'a>(sub: &'a FactSubstrate, root: u8) -> ReviewInput<'a> {
        ReviewInput {
            substrate: sub,
            store_root: h(root),
            target: "F",
        }
    }

    #[test]
    fn names() {
        assert_eq!(NoCache.name(), "no-cache");
        assert_eq!(WholeInput.name(), "whole-input");
        assert_eq!(RestrictionKey.name(), "restriction");
    }

    #[test]
    fn no_cache_has_no_key() {
        let s = substrate("w");
        assert_eq!(NoCache.key(&input(&s, 0), &mut 0), None);
    }

    #[test]
    fn whole_input_keys_on_root_parser_and_target() {
        let s = substrate("w");
        let mut touched = 0;
        assert_eq!(
            WholeInput.key(&input(&s, 5), &mut touched),
            Some(whole_input_key(h(5), h(1), "F"))
        );
        assert_eq!(touched, 0, "whole-input reads no rows");
        assert_ne!(
            WholeInput.key(&input(&s, 5), &mut 0),
            WholeInput.key(&input(&s, 6), &mut 0)
        );
    }

    #[test]
    fn restriction_keys_on_the_closure_and_ignores_the_root() {
        let s = substrate("w");
        let mut touched = 0;
        let k = RestrictionKey.key(&input(&s, 5), &mut touched);
        assert_eq!(k, Some(restriction_for_call_target(&s, "F", &mut 0)));
        assert_eq!(touched, 2, "one ref, one def");
        assert_eq!(k, RestrictionKey.key(&input(&s, 6), &mut 0));
        let other = substrate("v");
        assert_ne!(k, RestrictionKey.key(&input(&other, 5), &mut 0));
    }

    #[test]
    fn cache_stores_and_serves_by_key() {
        let mut c: ReviewCache<u32> = ReviewCache::default();
        assert!(c.is_empty());
        c.insert(None, 1);
        assert_eq!(c.len(), 0, "a None key stores nothing");
        assert_eq!(c.get(None), None);
        c.insert(Some(h(1)), 2);
        assert_eq!(c.len(), 1);
        assert!(!c.is_empty());
        assert_eq!(c.get(Some(&h(1))), Some(&2));
        assert_eq!(c.get(Some(&h(2))), None);
    }

    #[test]
    fn get_or_compute_hits_after_a_miss() {
        let mut c: ReviewCache<u32> = ReviewCache::new();
        let mut calls = 0;
        let (v, hit) = c.get_or_compute(Some(h(1)), || {
            calls += 1;
            7
        });
        assert_eq!((v, hit), (7, false));
        let (v, hit) = c.get_or_compute(Some(h(1)), || {
            calls += 1;
            8
        });
        assert_eq!((v, hit), (7, true));
        assert_eq!(calls, 1);
        let (v, hit) = c.get_or_compute(None, || 9);
        assert_eq!((v, hit), (9, false));
        assert_eq!(c.len(), 1);
    }
}
