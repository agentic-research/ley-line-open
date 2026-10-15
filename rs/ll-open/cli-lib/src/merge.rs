//! ADR-0045 T5 — the three-way merge a proposal against a moved base gets.
//!
//! `base` is the blob the writer read, `ours` the bytes it proposes,
//! `theirs` what the path holds now. The merge is `gix-merge`'s builtin
//! text driver (Myers over lines), run in process. A clean merge is the
//! proposal's write; overlapping hunks are a conflict, reported as the base
//! line ranges where the two sides overlap. Bytes that are not UTF-8 never
//! merge.

/// A range of the base's lines, 0-based and half-open: `[start, end)`.
/// Two insertions at one line are the empty range `[line, line)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// What a three-way merge produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Merged {
    /// Both sides' changes, in one file.
    Clean(Vec<u8>),
    /// The two sides changed overlapping base lines.
    Conflict(Vec<LineRange>),
    /// A side is not UTF-8: nothing is merged.
    Binary,
}

/// Merge `ours` and `theirs`, both changed from `base`.
pub fn merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merged {
    use gix_merge::blob::{Resolution, builtin_driver::text};
    if [base, ours, theirs]
        .iter()
        .any(|side| std::str::from_utf8(side).is_err())
    {
        return Merged::Binary;
    }
    let mut out = Vec::new();
    let mut input = imara_diff::InternedInput::default();
    let resolution = gix_merge::blob::builtin_driver::text(
        &mut out,
        &mut input,
        text::Labels::default(),
        ours,
        base,
        theirs,
        text::Options::default(),
    );
    match resolution {
        Resolution::Complete | Resolution::CompleteWithAutoResolvedConflict => Merged::Clean(out),
        Resolution::Conflict => {
            let ranges = conflict_ranges(base, ours, theirs);
            // gix-merge found a conflict; when the overlap rule below names
            // none, the whole base is named rather than nothing.
            Merged::Conflict(if ranges.is_empty() {
                vec![LineRange {
                    start: 0,
                    end: base_lines(base),
                }]
            } else {
                ranges
            })
        }
    }
}

fn base_lines(base: &[u8]) -> u32 {
    imara_diff::sources::byte_lines(base).count() as u32
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Ours,
    Theirs,
}

#[derive(Clone)]
struct Hunk {
    before: std::ops::Range<u32>,
    side: Side,
}

/// The hunks of `base` → `side`, as `gix-merge` computes them: Myers over
/// line tokens, post-processed for lines.
fn hunks(base: &[u8], side_bytes: &[u8], side: Side) -> Vec<Hunk> {
    let input = imara_diff::InternedInput::new(
        imara_diff::sources::byte_lines(base),
        imara_diff::sources::byte_lines(side_bytes),
    );
    let mut diff = imara_diff::Diff::compute(imara_diff::Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|h| Hunk {
            before: h.before,
            side,
        })
        .collect()
}

/// The base ranges where a hunk of one side overlaps a hunk of the other,
/// clustered with `gix-merge`'s rule (`take_intersecting`): a hunk overlaps
/// another of the other side when the other's base range contains its
/// start, or both are insertions at one line.
fn conflict_ranges(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<LineRange> {
    fn overlaps(left: &Hunk, right: &Hunk) -> bool {
        left.side != right.side
            && (right.before.contains(&left.before.start)
                || (right.before.is_empty() && right.before.start == left.before.start))
    }
    let mut all = hunks(base, ours, Side::Ours);
    all.extend(hunks(base, theirs, Side::Theirs));
    all.sort_by_key(|h| h.before.start);
    let mut iter = all.into_iter().peekable();
    let mut out: Vec<LineRange> = Vec::new();
    while let Some(first) = iter.next() {
        let mut taken = vec![first];
        let mut intersecting: Vec<Hunk> = Vec::new();
        loop {
            let last = taken.last().expect("one pushed").clone();
            while let Some(h) = iter.next_if(|h| overlaps(h, &last)) {
                intersecting.push(h);
            }
            let mut more = false;
            while let Some(last_i) = intersecting.last().cloned()
                && let Some(h) = iter.next_if(|h| overlaps(h, &last_i))
            {
                taken.push(h);
                more = true;
            }
            if !more {
                break;
            }
        }
        if intersecting.is_empty() {
            continue;
        }
        let both = taken.iter().chain(&intersecting);
        let start = both.clone().map(|h| h.before.start).min().unwrap_or(0);
        let end = both.map(|h| h.before.end).max().unwrap_or(start);
        out.push(LineRange { start, end });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "package a\n\nfunc A() int { return 1 }\n\nfunc B() int { return 2 }\n\nfunc C() int { return 3 }\n";

    fn edit(line: &str, with: &str) -> String {
        BASE.replace(line, with)
    }

    #[test]
    fn disjoint_edits_merge_cleanly_into_both() {
        let ours = edit("return 1", "return 10");
        let theirs = edit("return 3", "return 30");
        let Merged::Clean(out) = merge(BASE.as_bytes(), ours.as_bytes(), theirs.as_bytes()) else {
            panic!("disjoint edits merge");
        };
        assert_eq!(
            String::from_utf8(out).unwrap(),
            BASE.replace("return 1", "return 10")
                .replace("return 3", "return 30")
        );
    }

    #[test]
    fn the_same_change_on_both_sides_merges_to_it() {
        let both = edit("return 2", "return 20");
        assert_eq!(
            merge(BASE.as_bytes(), both.as_bytes(), both.as_bytes()),
            Merged::Clean(both.into_bytes())
        );
    }

    #[test]
    fn an_unchanged_side_takes_the_other() {
        let ours = edit("return 2", "return 20");
        assert_eq!(
            merge(BASE.as_bytes(), ours.as_bytes(), BASE.as_bytes()),
            Merged::Clean(ours.clone().into_bytes())
        );
        assert_eq!(
            merge(BASE.as_bytes(), BASE.as_bytes(), ours.as_bytes()),
            Merged::Clean(ours.into_bytes())
        );
    }

    /// Both sides rewrite line 5 (0-based 4): one conflict naming it.
    #[test]
    fn overlapping_edits_conflict_on_the_base_lines_they_share() {
        let ours = edit("return 2", "return 20");
        let theirs = edit("return 2", "return 200");
        assert_eq!(
            merge(BASE.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 4, end: 5 }])
        );
    }

    /// Two conflicts and a clean change between them: each conflict is
    /// named, and nothing else.
    #[test]
    fn each_conflict_is_named_and_clean_changes_are_not() {
        let ours = BASE
            .replace("return 1", "return 10")
            .replace("func B", "func Bee")
            .replace("return 3", "return 30");
        let theirs = BASE
            .replace("return 1", "return 11")
            .replace("return 3", "return 33");
        assert_eq!(
            merge(BASE.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![
                LineRange { start: 2, end: 3 },
                LineRange { start: 6, end: 7 },
            ])
        );
    }

    /// Two insertions at one place conflict on the empty range there.
    #[test]
    fn two_insertions_at_one_line_conflict() {
        let ours = format!("{BASE}func D() {{}}\n");
        let theirs = format!("{BASE}func E() {{}}\n");
        assert_eq!(
            merge(BASE.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 7, end: 7 }])
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_never_merge() {
        let bad = b"\xff\xfe binary\n";
        assert_eq!(merge(bad, BASE.as_bytes(), BASE.as_bytes()), Merged::Binary);
        assert_eq!(merge(BASE.as_bytes(), bad, BASE.as_bytes()), Merged::Binary);
        assert_eq!(merge(BASE.as_bytes(), BASE.as_bytes(), bad), Merged::Binary);
    }
}
