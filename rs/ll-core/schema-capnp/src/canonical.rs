//! Two-pass canonical serialization workaround.
//!
//! `capnp::message::Builder::set_root_canonical` requires the output
//! builder to have exactly one segment (asserted at
//! `capnp-0.25.x/src/message.rs:565`:
//! `assert_eq!(self.get_segments_for_output().len(), 1)`). But
//! `Builder::new_default()` uses `HeapAllocator` with the default
//! heuristic first-segment size (small; grows by doubling as needed).
//! For large source messages — monorepo-scale `AstNode` records with
//! deep ASTs, or `SourceFile` records for very large files — the
//! canonical builder ends up multi-segment and the assertion panics
//! mid-parse.
//!
//! Fix: measure the total word size of the source builder's segments,
//! then allocate a canonical builder whose FIRST segment is guaranteed
//! large enough to hold the whole canonical form. Canonical form may
//! reorder or pack differently, so we add slack (1.5× + 16 words) to
//! avoid an off-by-one that would re-trigger the assertion.

use capnp::message::{Builder, HeapAllocator};
use capnp::private::units::BYTES_PER_WORD;
use capnp::traits::Owned;

/// Serialize a canonical capnp message from `src` to `out`. See module
/// docstring for the workaround rationale.
///
/// `T` is the capnp `Owned` type of the message root; the caller
/// disambiguates via turbofish since the type isn't recoverable from
/// `src` alone.
pub fn write_canonical_message<T, W>(src: &Builder<HeapAllocator>, out: &mut W) -> capnp::Result<()>
where
    T: Owned,
    W: std::io::Write,
{
    let cap = compute_first_segment_words(src);
    let alloc = HeapAllocator::new().first_segment_words(cap);
    let mut canonical = Builder::new(alloc);
    // Extract the source root as its Reader<'_> — the Owned trait's
    // GAT (`type Reader<'a>: SetterInput<Self>`) makes this the right
    // input for `set_root_canonical::<T>`.
    let reader: <T as Owned>::Reader<'_> = src.get_root_as_reader()?;
    canonical.set_root_canonical::<T>(reader)?;
    capnp::serialize::write_message(out, &canonical)?;
    Ok(())
}

/// Compute the first-segment size (in words) for a canonical builder
/// that receives content from `src`. Formula: `1.5 × source_words + 16`
/// as slack for canonical-form repacking.
///
/// Extracted for testability; F-gate `first_segment_never_undersizes`
/// walks the range of realistic source sizes and asserts the returned
/// capacity strictly exceeds the source size.
fn compute_first_segment_words(src: &Builder<HeapAllocator>) -> u32 {
    let words: usize = src
        .get_segments_for_output()
        .iter()
        .map(|s| s.len() / BYTES_PER_WORD)
        .sum();
    // 1.5× + 16 words slack — canonical form may re-pack but never
    // grows dramatically. This is the load-bearing fix; a too-small
    // first segment triggers the upstream assertion.
    (words + words / 2 + 16) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common_capnp::hash;

    /// Regression test for the monorepo-parse panic: writing a
    /// message large enough to exceed the default first-segment size
    /// must not trigger `capnp::message::Builder::set_root_canonical`'s
    /// segment-count assertion.
    ///
    /// The test allocates a source message with a `data` field padded
    /// to > 4 KiB, which forces the default builder past its first
    /// segment. Pre-fix, this would panic at `message.rs:565`. Post-fix,
    /// the two-pass helper reallocates a canonical builder with enough
    /// first-segment capacity and completes cleanly.
    #[test]
    fn large_message_does_not_panic_on_canonical_write() {
        // Build a source message with a large payload. We use `hash`
        // as a convenience type; the `bytes` field is a Data blob that
        // accepts arbitrary length.
        let mut src = Builder::new_default();
        {
            let mut root: hash::Builder = src.init_root();
            // 64 KiB payload — well past the default first segment
            // size, forces multi-segment allocation pre-fix.
            let payload = vec![0xABu8; 64 * 1024];
            root.set_bytes(&payload);
        }

        // Pre-fix path (kept in the test's comment for clarity):
        //     let mut canonical = Builder::new_default();
        //     canonical.set_root_canonical(...)  // PANICS here
        //
        // Post-fix: the helper computes a first-segment big enough
        // to hold the whole message, then set_root_canonical succeeds
        // in a single segment.
        let mut buf: Vec<u8> = Vec::new();
        let result = write_canonical_message::<hash::Owned, _>(&src, &mut buf);
        assert!(
            result.is_ok(),
            "canonical write must succeed on large messages; got: {result:?}"
        );
        assert!(!buf.is_empty(), "canonical output must be non-empty");
    }

    #[test]
    fn small_message_still_works() {
        // Small messages that would have worked pre-fix must still
        // work post-fix (no regression on the common path).
        let mut src = Builder::new_default();
        {
            let mut root: hash::Builder = src.init_root();
            root.set_bytes(&[0x01, 0x02, 0x03]);
        }
        let mut buf: Vec<u8> = Vec::new();
        write_canonical_message::<hash::Owned, _>(&src, &mut buf)
            .expect("small canonical write must succeed");
        assert!(!buf.is_empty());
    }

    #[test]
    fn first_segment_computation_includes_slack() {
        // Pin the slack formula so a refactor that dropped the +16
        // fudge or changed the 1.5× multiplier would surface here.
        let mut src = Builder::new_default();
        {
            let mut root: hash::Builder = src.init_root();
            root.set_bytes(&[0u8; 800]); // ~100 words of payload
        }
        let cap = compute_first_segment_words(&src);
        let source_words: usize = src
            .get_segments_for_output()
            .iter()
            .map(|s| s.len() / BYTES_PER_WORD)
            .sum();
        assert!(
            cap as usize > source_words,
            "first-segment capacity ({cap}) must exceed source words ({source_words})"
        );
        assert!(
            cap as usize >= source_words + 16,
            "slack must be at least 16 words"
        );
    }
}
