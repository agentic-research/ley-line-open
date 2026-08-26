//! Wall-clock sampling for perf gates: minimum-of-N with interleaved arms.
//!
//! Every wall-clock perf gate in this workspace measures the same way, and
//! the technique lives here so the next gate cannot miss half of it. Two
//! rules, each covering a distinct measurement error:
//!
//! 1. **Take the MINIMUM of N samples — never one shot, never the mean.**
//!    Scheduler load is strictly additive: contention can make a run slower,
//!    never faster, so the minimum is the closest observable estimate of the
//!    code's true cost. A mean smears one preemption across every sample —
//!    at µs scale a single 1ms scheduling event is hundreds of iterations'
//!    worth of work — and a one-shot simply loses the runner lottery.
//!
//! 2. **When comparing two arms, INTERLEAVE them rep by rep.** Sampling arm
//!    A to completion and then arm B attributes any drift in machine load to
//!    whichever arm happened to be running during it. That error produced a
//!    bogus 0-vs-3 result during the `ley-line-open-cdd5d0` investigation,
//!    and a measured 0.98× throughput ratio on an idle machine in the F2
//!    gate — under its own 1.0 falsification bar — from two real samples
//!    taken minutes apart.
//!
//! History: the min-of-N started in `cold_parse_perf_regression.rs`; the F2
//! and sheaf-restriction gates flaked precisely because nothing shared it,
//! and the first fix copied the technique into each file — three
//! implementations of one idea (bead `ley-line-open-aae1c2`). This crate is
//! the extraction.
//!
//! The caller owns the timed region: arms return a [`Sample`], built either
//! with [`timed`] (the region is exactly the closure) or from a duration the
//! workload measured itself. Setup and post-run derivation (row counts, byte
//! totals) stay outside the region — see [`Sample::map`].

use std::time::{Duration, Instant};

/// One measured run: the wall time of the region the caller chose to time,
/// and whatever the run produced.
#[derive(Debug, Clone)]
pub struct Sample<T> {
    pub wall: Duration,
    pub value: T,
}

impl<T> Sample<T> {
    /// Replace the value, keeping the wall — for deriving a post-run
    /// measurement (row counts, byte totals) that must not be part of the
    /// timed region.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Sample<U> {
        Sample {
            wall: self.wall,
            value: f(self.value),
        }
    }
}

/// Time `f` and return its result as a [`Sample`]. The timed region is
/// exactly the closure — construct inputs before calling and derive
/// secondary measurements afterwards.
pub fn timed<T>(f: impl FnOnce() -> T) -> Sample<T> {
    let start = Instant::now();
    let value = f();
    Sample {
        wall: start.elapsed(),
        value,
    }
}

/// One arm's samples, in rep order, with the minimum-wall rep marked.
#[derive(Debug, Clone)]
pub struct BestOf<T> {
    samples: Vec<Sample<T>>,
    best: usize,
}

impl<T> BestOf<T> {
    fn from_samples(samples: Vec<Sample<T>>) -> Self {
        assert!(!samples.is_empty(), "best_of requires at least one rep");
        let mut best = 0;
        for (i, s) in samples.iter().enumerate() {
            if s.wall < samples[best].wall {
                best = i;
            }
        }
        BestOf { samples, best }
    }

    /// The minimum wall across reps — the estimate a gate asserts on.
    pub fn wall(&self) -> Duration {
        self.samples[self.best].wall
    }

    /// The value produced by the minimum-wall rep.
    pub fn value(&self) -> &T {
        &self.samples[self.best].value
    }

    /// Every rep's wall, in rep order — for the diagnostic line a gate
    /// prints so CI logs carry the spread, not just the minimum.
    pub fn walls(&self) -> Vec<Duration> {
        self.samples.iter().map(|s| s.wall).collect()
    }

    /// Every rep's sample, in rep order.
    pub fn samples(&self) -> &[Sample<T>] {
        &self.samples
    }
}

/// Run one arm `reps` times and keep the minimum-wall sample (rule 1 of the
/// crate docs). The closure receives the rep index; ties keep the earliest
/// rep. Panics if `reps` is zero.
pub fn best_of<T>(reps: usize, run: impl FnMut(usize) -> Sample<T>) -> BestOf<T> {
    BestOf::from_samples((0..reps).map(run).collect())
}

/// Run two arms `reps` times each, interleaved `a, b, a, b, …` so load
/// drift lands on both arms instead of whichever ran last (rule 2 of the
/// crate docs), and keep each arm's minimum-wall sample. Panics if `reps`
/// is zero.
pub fn best_of_interleaved<A, B>(
    reps: usize,
    mut a: impl FnMut(usize) -> Sample<A>,
    mut b: impl FnMut(usize) -> Sample<B>,
) -> (BestOf<A>, BestOf<B>) {
    let mut samples_a = Vec::with_capacity(reps);
    let mut samples_b = Vec::with_capacity(reps);
    for rep in 0..reps {
        samples_a.push(a(rep));
        samples_b.push(b(rep));
    }
    (
        BestOf::from_samples(samples_a),
        BestOf::from_samples(samples_b),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn best_of_picks_the_minimum_wall_and_its_value() {
        let walls = [30u64, 10, 20];
        let result = best_of(3, |rep| Sample {
            wall: ms(walls[rep]),
            value: rep,
        });
        assert_eq!(result.wall(), ms(10));
        assert_eq!(*result.value(), 1);
        assert_eq!(result.walls(), vec![ms(30), ms(10), ms(20)]);
        assert_eq!(result.samples().len(), 3);
    }

    #[test]
    fn ties_keep_the_earliest_rep() {
        let result = best_of(2, |rep| Sample {
            wall: ms(10),
            value: rep,
        });
        assert_eq!(*result.value(), 0);
    }

    #[test]
    #[should_panic(expected = "at least one rep")]
    fn zero_reps_panics() {
        let _ = best_of(0, |rep| Sample {
            wall: ms(1),
            value: rep,
        });
    }

    #[test]
    fn interleaved_alternates_arms_within_each_rep() {
        // The regression this API exists to prevent is sequential arms
        // (a,a,a then b,b,b), so pin the exact schedule.
        let log = RefCell::new(Vec::new());
        let (a, b) = best_of_interleaved(
            3,
            |rep| {
                log.borrow_mut().push(format!("a{rep}"));
                Sample {
                    wall: ms(rep as u64 + 1),
                    value: (),
                }
            },
            |rep| {
                log.borrow_mut().push(format!("b{rep}"));
                Sample {
                    wall: ms(rep as u64 + 1),
                    value: (),
                }
            },
        );
        assert_eq!(log.into_inner(), ["a0", "b0", "a1", "b1", "a2", "b2"]);
        assert_eq!(a.wall(), ms(1));
        assert_eq!(b.wall(), ms(1));
    }

    #[test]
    fn timed_wall_covers_at_least_the_closure_work() {
        // The closure spins on the same monotonic clock `timed` reads until
        // 5ms have verifiably passed inside it, so the bound cannot flake
        // slow. Not `thread::sleep`: the `sleep_in_tests` smell gate
        // rejects sleeps because as a synchronization device they race the
        // scheduler — here the elapsed time IS the fixture, and the spin
        // states that directly. Falsifies a `timed` that measures the
        // wrong region, which would report near-zero.
        let s = timed(|| {
            let start = Instant::now();
            while start.elapsed() < ms(5) {
                std::hint::spin_loop();
            }
            42
        });
        assert!(
            s.wall >= ms(5),
            "wall {:?} < the 5ms the closure verifiably spun",
            s.wall
        );
        assert_eq!(s.value, 42);
    }

    #[test]
    fn map_replaces_value_and_keeps_wall() {
        let s = Sample {
            wall: ms(7),
            value: 2,
        }
        .map(|v| v * 10);
        assert_eq!(s.wall, ms(7));
        assert_eq!(s.value, 20);
    }
}
