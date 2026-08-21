//! Interrupt flag constants and helpers for the Controller control block.
//!
//! Level-triggered bitmask: writers set bits with `fetch_or(Release)`,
//! readers check with `load(Acquire)` and clear handled bits with
//! `fetch_and(!bits, Release)`. A set bit stays set until the handler
//! explicitly clears it — no lost signals, no queue capacity issues.

/// One interrupt bit, by index.
///
/// Routing every constant through this keeps the `1 << n` shape that says
/// these are a flag mask, while removing a shift-by-ZERO special case:
/// `1 << 0` and `1 >> 0` are the same integer, so `HALT`'s literal shift
/// carried a mutation no test could observe and no restructuring of that one
/// line could remove (bead `ley-line-open-b23c41`). With a variable shift
/// there is exactly one `<<` in the module, and flipping it moves every bit
/// except `HALT` — which `bits_are_distinct` sees immediately.
const fn bit(index: u32) -> u64 {
    1 << index
}

/// Stop generation, discard partial output.
pub const HALT: u64 = bit(0);

/// Freeze generation, preserve KV cache for later resume.
pub const PAUSE: u64 = bit(1);

/// New context available in sidecar shm; restart with prefix-sharing.
pub const REDIRECT: u64 = bit(2);

/// Abort the currently executing tool call.
pub const CANCEL_TOOL: u64 = bit(3);

/// Escalate the current task's scheduling priority.
pub const PRIORITY_BUMP: u64 = bit(4);

/// Khipu detected a semantic/sheaf coherence violation.
pub const COHERENCE_ALERT: u64 = bit(5);

/// Memory or compute pressure — shed non-critical work.
pub const RESOURCE_PRESSURE: u64 = bit(6);

/// Liveness probe — handler should respond with a heartbeat.
pub const HEARTBEAT_REQ: u64 = bit(7);

/// Human-readable name for a single interrupt bit.
pub fn bit_name(bit: u64) -> &'static str {
    match bit {
        HALT => "HALT",
        PAUSE => "PAUSE",
        REDIRECT => "REDIRECT",
        CANCEL_TOOL => "CANCEL_TOOL",
        PRIORITY_BUMP => "PRIORITY_BUMP",
        COHERENCE_ALERT => "COHERENCE_ALERT",
        RESOURCE_PRESSURE => "RESOURCE_PRESSURE",
        HEARTBEAT_REQ => "HEARTBEAT_REQ",
        _ => "UNKNOWN",
    }
}

/// Iterate over the set bits in a flags word, yielding each bit value.
pub fn iter_set_bits(flags: u64) -> impl Iterator<Item = u64> {
    (0..64)
        .map(|i| 1u64 << i)
        .filter(move |bit| flags & bit != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_distinct() {
        let all = HALT
            | PAUSE
            | REDIRECT
            | CANCEL_TOOL
            | PRIORITY_BUMP
            | COHERENCE_ALERT
            | RESOURCE_PRESSURE
            | HEARTBEAT_REQ;
        assert_eq!(all.count_ones(), 8);
    }

    #[test]
    fn iter_set_bits_roundtrip() {
        let flags = HALT | REDIRECT | HEARTBEAT_REQ;
        let collected: Vec<u64> = iter_set_bits(flags).collect();
        assert_eq!(collected, vec![HALT, REDIRECT, HEARTBEAT_REQ]);
    }

    /// The full bit -> name table, not a sample of it.
    ///
    /// The previous version asserted `HALT` and `COHERENCE_ALERT` and stopped.
    /// Deleting any ONE of the other six match arms therefore changed nothing
    /// any test could see — the deleted bit falls through to `_ => "UNKNOWN"`,
    /// and no assertion looked. Six surviving mutants, one per unasserted arm
    /// (bead `ley-line-open-b23c41`).
    ///
    /// A name is what an operator reads out of a control-block dump to decide
    /// what a stuck agent is waiting on, so a bit that reports `UNKNOWN` — or
    /// worse, another bit's name — is a debugging dead end.
    #[test]
    fn every_bit_resolves_to_its_own_name() {
        const NAMED: [(u64, &str); 8] = [
            (HALT, "HALT"),
            (PAUSE, "PAUSE"),
            (REDIRECT, "REDIRECT"),
            (CANCEL_TOOL, "CANCEL_TOOL"),
            (PRIORITY_BUMP, "PRIORITY_BUMP"),
            (COHERENCE_ALERT, "COHERENCE_ALERT"),
            (RESOURCE_PRESSURE, "RESOURCE_PRESSURE"),
            (HEARTBEAT_REQ, "HEARTBEAT_REQ"),
        ];

        for (bit, name) in NAMED {
            assert_eq!(
                bit_name(bit),
                name,
                "bit {bit:#x} must resolve to its OWN name, not another arm's \
                 and not UNKNOWN"
            );
        }

        // Catches a constant added without a matching arm: the new bit would
        // fall through to UNKNOWN, and the table above would not mention it.
        let all = HALT
            | PAUSE
            | REDIRECT
            | CANCEL_TOOL
            | PRIORITY_BUMP
            | COHERENCE_ALERT
            | RESOURCE_PRESSURE
            | HEARTBEAT_REQ;
        assert_eq!(
            all.count_ones() as usize,
            NAMED.len(),
            "every defined bit must appear in this table — a constant added \
             without an arm reads as UNKNOWN in a control-block dump"
        );
        for bit in iter_set_bits(all) {
            assert_ne!(
                bit_name(bit),
                "UNKNOWN",
                "bit {bit:#x} is defined but has no name"
            );
        }

        // The fallthrough still has to work.
        assert_eq!(bit_name(1 << 32), "UNKNOWN");
        assert_eq!(bit_name(0), "UNKNOWN", "the empty mask names nothing");
    }
}
