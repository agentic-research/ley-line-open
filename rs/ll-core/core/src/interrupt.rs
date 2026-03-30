//! Interrupt flag constants and helpers for the Controller control block.
//!
//! Level-triggered bitmask: writers set bits with `fetch_or(Release)`,
//! readers check with `load(Acquire)` and clear handled bits with
//! `fetch_and(!bits, Release)`. A set bit stays set until the handler
//! explicitly clears it — no lost signals, no queue capacity issues.

/// Stop generation, discard partial output.
pub const HALT: u64 = 1 << 0;

/// Freeze generation, preserve KV cache for later resume.
pub const PAUSE: u64 = 1 << 1;

/// New context available in sidecar shm; restart with prefix-sharing.
pub const REDIRECT: u64 = 1 << 2;

/// Abort the currently executing tool call.
pub const CANCEL_TOOL: u64 = 1 << 3;

/// Escalate the current task's scheduling priority.
pub const PRIORITY_BUMP: u64 = 1 << 4;

/// Khipu detected a semantic/sheaf coherence violation.
pub const COHERENCE_ALERT: u64 = 1 << 5;

/// Memory or compute pressure — shed non-critical work.
pub const RESOURCE_PRESSURE: u64 = 1 << 6;

/// Liveness probe — handler should respond with a heartbeat.
pub const HEARTBEAT_REQ: u64 = 1 << 7;

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

    #[test]
    fn bit_names_resolve() {
        assert_eq!(bit_name(HALT), "HALT");
        assert_eq!(bit_name(COHERENCE_ALERT), "COHERENCE_ALERT");
        assert_eq!(bit_name(1 << 32), "UNKNOWN");
    }
}
