//! The ablation driver (bead `ley-line-open-754aa1`): one replay, several
//! cache arms, the same metrics for each.
//!
//! A replay is a sequence of steps, each a `(before, after)` corpus pair (a
//! git commit and its parent, or two synthetic versions). For every step the
//! driver extracts both substrates once, aligns the function regions present
//! on both sides, and computes the review of every aligned function on both
//! sides once: those values are the oracle. Then, for each arm:
//!
//! 1. **warm**: the arm sees the before state and caches its values — the
//!    state a daemon holds at the parent commit. A warm lookup that hits is
//!    already checked against the oracle;
//! 2. **measure**: the arm sees the after state and is asked for every
//!    aligned function. A served value is a HIT; it is compared with the
//!    recomputed oracle, and a difference is a FALSE SKIP. A miss whose
//!    before and after values are equal is a FALSE MISS (the arm
//!    invalidated a value that had not changed).
//!
//! Counts are deterministic. Timings are measured and reported, never
//! asserted. End-to-end time is `key/invalidation time + lookup time +` the
//! measured review time of every miss (each review is timed once per step and
//! charged to every arm that misses it, so arms are charged identical review
//! costs).

use super::{ReviewTarget, build_substrate, review_targets};
use leyline_cache::restriction::{
    ContainerKeying, FactSubstrate, ResolvedEdge, review_call_targets, whole_object_hash,
};
use leyline_cache::strategy::{KeyStrategy, ReviewCache, ReviewInput};
use leyline_core::Hash;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// A review value: the resolved call graph of one function.
pub type Value = BTreeSet<ResolvedEdge>;

/// One aligned function region of a step.
pub struct Target {
    pub source_id: String,
    pub name: String,
    pub occurrence: u32,
    pub container_before: String,
    pub container_after: String,
    pub bytes_before: Vec<u8>,
    pub bytes_after: Vec<u8>,
    pub value_before: Value,
    pub value_after: Value,
    /// Measured time of one after-state review.
    pub review_time: Duration,
}

/// Everything an arm may read about one step.
pub struct Step {
    pub label: String,
    pub before: FactSubstrate,
    pub after: FactSubstrate,
    pub root_before: Hash,
    pub root_after: Hash,
    pub targets: Vec<Target>,
}

/// One arm of the ablation.
pub trait Arm {
    fn name(&self) -> String;
    /// Whether the arm claims soundness (its false skips must be 0).
    fn claims_soundness(&self) -> bool;
    /// See the before state and cache its values. Returns the number of
    /// warm hits whose cached value differed from the oracle.
    fn warm(&mut self, step: &Step) -> u64;
    /// Prepare to answer for the after state (invalidation-driven arms do
    /// their work here). Timed as key cost.
    fn prepare(&mut self, _step: &Step) {}
    /// The value served for target `i` of the after state, or `None` on a
    /// miss. Timed as key cost.
    fn lookup(&mut self, step: &Step, i: usize) -> Option<&Value>;
    /// Store the recomputed after-state value of target `i` after a miss.
    fn store(&mut self, step: &Step, i: usize);
    /// Substrate rows read computing keys, if the arm counts them.
    fn key_rows(&self) -> u64 {
        0
    }
}

/// A key-strategy arm: [`ReviewCache`] under a [`KeyStrategy`]. The cache
/// persists across steps, so a key seen in an earlier step can hit later.
pub struct KeyArm<S: KeyStrategy> {
    pub strategy: S,
    pub cache: ReviewCache<Value>,
    pub rows: u64,
    pending: Vec<Option<Hash>>,
}

impl<S: KeyStrategy> KeyArm<S> {
    pub fn new(strategy: S) -> Self {
        KeyArm {
            strategy,
            cache: ReviewCache::new(),
            rows: 0,
            pending: Vec::new(),
        }
    }
}

fn input<'a>(sub: &'a FactSubstrate, root: Hash, target: &'a str) -> ReviewInput<'a> {
    ReviewInput {
        substrate: sub,
        tree_root: root,
        target,
    }
}

impl<S: KeyStrategy> Arm for KeyArm<S> {
    fn name(&self) -> String {
        self.strategy.name().to_string()
    }
    fn claims_soundness(&self) -> bool {
        true
    }
    fn warm(&mut self, step: &Step) -> u64 {
        let mut bad = 0;
        let mut scratch = 0;
        for t in &step.targets {
            let key = self.strategy.key(
                &input(&step.before, step.root_before, &t.container_before),
                &mut scratch,
            );
            match self.cache.get(key.as_ref()) {
                Some(v) if *v != t.value_before => bad += 1,
                Some(_) => {}
                None => self.cache.insert(key, t.value_before.clone()),
            }
        }
        bad
    }
    fn prepare(&mut self, step: &Step) {
        self.pending = vec![None; step.targets.len()];
    }
    fn lookup(&mut self, step: &Step, i: usize) -> Option<&Value> {
        let t = &step.targets[i];
        let key = self.strategy.key(
            &input(&step.after, step.root_after, &t.container_after),
            &mut self.rows,
        );
        self.pending[i] = key;
        self.cache.get(key.as_ref())
    }
    fn store(&mut self, step: &Step, i: usize) {
        self.cache
            .insert(self.pending[i], step.targets[i].value_after.clone());
    }
    fn key_rows(&self) -> u64 {
        self.rows
    }
}

/// Per-arm results.
#[derive(Default, Clone)]
pub struct ArmStats {
    pub name: String,
    pub claims_soundness: bool,
    pub lookups: u64,
    pub hits: u64,
    pub false_skips: u64,
    pub warm_false_skips: u64,
    pub false_misses: u64,
    pub value_changed: u64,
    pub key_rows: u64,
    pub key_time: Duration,
    pub e2e_time: Duration,
    /// `step / file / fn#occurrence` of every false skip, never suppressed.
    pub false_skip_log: Vec<String>,
}

/// Alignment key of a function region.
type AlignKey = (String, String, u32);

fn index(targets: Vec<ReviewTarget>) -> (BTreeMap<AlignKey, ReviewTarget>, BTreeMap<String, u32>) {
    let mut by_key = BTreeMap::new();
    let mut mult: BTreeMap<String, u32> = BTreeMap::new();
    for t in targets {
        *mult.entry(t.container.clone()).or_insert(0) += 1;
        by_key.insert((t.source_id.clone(), t.name.clone(), t.occurrence), t);
    }
    (by_key, mult)
}

/// Build one step. Functions present on both sides are aligned by
/// `(file, name, occurrence)`; a function whose stable container id is not
/// unique on either side is excluded (`Stable` keying is body-invariant, so
/// same-signature functions merge their refs under one container). Returns
/// the step and the number of exclusions.
pub fn build_step(
    label: &str,
    before_corpus: &[(String, String)],
    after_corpus: &[(String, String)],
) -> (Step, u64) {
    let before = build_substrate(before_corpus, ContainerKeying::Stable);
    let after = build_substrate(after_corpus, ContainerKeying::Stable);
    let (b_by, b_mult) = index(review_targets(before_corpus, ContainerKeying::Stable));
    let (a_by, a_mult) = index(review_targets(after_corpus, ContainerKeying::Stable));
    let src = |c: &[(String, String)]| -> BTreeMap<String, String> { c.iter().cloned().collect() };
    let (b_src, a_src) = (src(before_corpus), src(after_corpus));

    let mut excluded = 0;
    let mut targets = Vec::new();
    for (key, tb) in &b_by {
        let Some(ta) = a_by.get(key) else { continue };
        if b_mult[&tb.container] != 1 || a_mult[&ta.container] != 1 {
            excluded += 1;
            continue;
        }
        let mut scratch = 0;
        let value_before = review_call_targets(&before, &tb.container, &mut scratch);
        let t0 = Instant::now();
        let value_after = review_call_targets(&after, &ta.container, &mut scratch);
        let review_time = t0.elapsed();
        targets.push(Target {
            source_id: key.0.clone(),
            name: key.1.clone(),
            occurrence: key.2,
            container_before: tb.container.clone(),
            container_after: ta.container.clone(),
            bytes_before: b_src[&tb.source_id].as_bytes()[tb.start..tb.end].to_vec(),
            bytes_after: a_src[&ta.source_id].as_bytes()[ta.start..ta.end].to_vec(),
            value_before,
            value_after,
            review_time,
        });
    }
    let step = Step {
        label: label.to_string(),
        root_before: whole_object_hash(before_corpus),
        root_after: whole_object_hash(after_corpus),
        before,
        after,
        targets,
    };
    (step, excluded)
}

/// Run every arm over one step, accumulating into `stats` (same order as
/// `arms`).
pub fn run_step(step: &Step, arms: &mut [Box<dyn Arm>], stats: &mut Vec<ArmStats>) {
    if stats.is_empty() {
        for arm in arms.iter() {
            stats.push(ArmStats {
                name: arm.name(),
                claims_soundness: arm.claims_soundness(),
                ..ArmStats::default()
            });
        }
    }
    for (arm, s) in arms.iter_mut().zip(stats.iter_mut()) {
        s.warm_false_skips += arm.warm(step);
        let t0 = Instant::now();
        arm.prepare(step);
        let prep = t0.elapsed();
        s.key_time += prep;
        s.e2e_time += prep;
        for (i, t) in step.targets.iter().enumerate() {
            s.lookups += 1;
            let changed = t.value_before != t.value_after;
            if changed {
                s.value_changed += 1;
            }
            let t0 = Instant::now();
            let served = arm.lookup(step, i);
            let k = t0.elapsed();
            s.key_time += k;
            s.e2e_time += k;
            match served {
                Some(v) => {
                    s.hits += 1;
                    if *v != t.value_after {
                        s.false_skips += 1;
                        s.false_skip_log.push(format!(
                            "{} {} {}#{}",
                            step.label, t.source_id, t.name, t.occurrence
                        ));
                    }
                }
                None => {
                    let t0 = Instant::now();
                    arm.store(step, i);
                    s.e2e_time += t0.elapsed() + t.review_time;
                    if !changed {
                        s.false_misses += 1;
                    }
                }
            }
        }
        s.key_rows = arm.key_rows();
    }
}

/// Print the ablation table.
pub fn print_table(stats: &[ArmStats]) {
    println!(
        "{:<22} {:>8} {:>8} {:>9} {:>11} {:>11} {:>12} {:>12} {:>12}",
        "arm",
        "lookups",
        "hits",
        "hit rate",
        "false skip",
        "false miss",
        "key rows",
        "key time",
        "e2e time"
    );
    println!("{}", "-".repeat(113));
    for s in stats {
        let rate = s.hits as f64 / s.lookups.max(1) as f64;
        println!(
            "{:<22} {:>8} {:>8} {:>8.1}% {:>11} {:>11} {:>12} {:>12.2?} {:>12.2?}",
            s.name,
            s.lookups,
            s.hits,
            100.0 * rate,
            s.false_skips + s.warm_false_skips,
            s.false_misses,
            s.key_rows,
            s.key_time,
            s.e2e_time
        );
    }
    if let Some(s) = stats.first() {
        println!(
            "value changed on {} of {} lookups (the oracle; identical for every arm)",
            s.value_changed, s.lookups
        );
    }
    for s in stats {
        for line in &s.false_skip_log {
            println!("  FALSE SKIP [{}] {line}", s.name);
        }
    }
}
