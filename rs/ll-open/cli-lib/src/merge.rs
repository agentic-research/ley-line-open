//! ADR-0045 T5 — the three-way merge a proposal against a moved base gets.
//!
//! `base` is the blob the writer read, `ours` the bytes it proposes,
//! `theirs` what the path holds now. The merge is gix-merge's builtin text
//! driver (Myers over lines, git's rule), run in process. A clean merge is
//! the proposal's write; hunks of the two sides that overlap or touch are a
//! conflict, reported as the base line ranges where they collide. Bytes
//! that are not UTF-8 never merge.

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
    /// The two sides changed overlapping or touching base lines; never
    /// empty.
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
    let resolution = text(
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
        // The ranges are computed with the driver's own region rule over the
        // same hunks, so a conflict names at least one
        // (`a_conflict_always_names_its_base_ranges`).
        Resolution::Conflict => Merged::Conflict(conflict_ranges(base, ours, theirs)),
    }
}

/// One hunk of a line diff `from` → `to`: the lines of `from` it replaces
/// and the lines of `to` that replace them, each 0-based and half-open. An
/// insertion has an empty `from` range; a deletion an empty `to` range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Change {
    pub from: LineRange,
    pub to: LineRange,
}

/// The hunks of the line diff `from` → `to` (Myers over lines,
/// post-processed for lines, as [`merge`] diffs). Bytes need not be UTF-8:
/// lines are split on `\n`.
pub fn changed_lines(from: &[u8], to: &[u8]) -> Vec<Change> {
    let input = imara_diff::InternedInput::new(
        imara_diff::sources::byte_lines(from),
        imara_diff::sources::byte_lines(to),
    );
    let mut diff = imara_diff::Diff::compute(imara_diff::Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|h| Change {
            from: LineRange {
                start: h.before.start,
                end: h.before.end,
            },
            to: LineRange {
                start: h.after.start,
                end: h.after.end,
            },
        })
        .collect()
}

/// One hunk of `base` → a side, in base lines and that side's lines.
struct Hunk {
    before: std::ops::Range<u32>,
    after: std::ops::Range<u32>,
    ours: bool,
}

/// The hunks of `base` → `side` as the driver computes them: Myers over
/// line tokens, post-processed without the indent heuristic (which can move
/// overlapping edits apart and hide a conflict).
fn hunks(base: &[u8], side: &[u8], ours: bool) -> Vec<Hunk> {
    let input = imara_diff::InternedInput::new(
        imara_diff::sources::byte_lines(base),
        imara_diff::sources::byte_lines(side),
    );
    let mut diff = imara_diff::Diff::compute(imara_diff::Algorithm::Myers, &input);
    diff.postprocess_no_heuristic(&input);
    diff.hunks()
        .map(|h| Hunk {
            before: h.before,
            after: h.after,
            ours,
        })
        .collect()
}

/// The lines of `bytes`, each with its `\n`, as the driver tokenizes them.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|b| *b == b'\n').collect()
}

/// The base ranges the driver conflicts on, by its own rule (gix-merge
/// 0.24, `regions`, as git's xdiff merges): the hunks of both sides, in
/// base order, group while a hunk starts at or before the group's end, so
/// hunks that only touch (a change and an insertion at its edge, two
/// insertions at one line) are one region. A region both sides changed, to
/// different lines, is a conflict; its base range is the region's.
fn conflict_ranges(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<LineRange> {
    let (ours_lines, theirs_lines) = (lines(ours), lines(theirs));
    let mut all = hunks(base, ours, true);
    all.extend(hunks(base, theirs, false));
    all.sort_by_key(|h| h.before.start);
    let mut all = all.into_iter().peekable();
    let mut out = Vec::new();
    // Where the previous region ended, in the base and in each side.
    let (mut base_end, mut ours_end, mut theirs_end) = (0u32, 0u32, 0u32);
    while let Some(first) = all.next() {
        let start = first.before.start;
        let gap = start - base_end;
        let (ours_start, theirs_start) = (ours_end + gap, theirs_end + gap);
        let mut end = first.before.end;
        let mut group = vec![first];
        while let Some(h) = all.next_if(|h| h.before.start <= end) {
            end = end.max(h.before.end);
            group.push(h);
        }
        ours_end = ours_start + (end - start);
        theirs_end = theirs_start + (end - start);
        let (mut ours_changed, mut theirs_changed) = (false, false);
        for h in &group {
            let (side_end, changed) = if h.ours {
                (&mut ours_end, &mut ours_changed)
            } else {
                (&mut theirs_end, &mut theirs_changed)
            };
            // The region spans each hunk's base lines, so this never underflows.
            *side_end = *side_end + (h.after.end - h.after.start) - (h.before.end - h.before.start);
            *changed = true;
        }
        let differ = ours_lines.get(ours_start as usize..ours_end as usize)
            != theirs_lines.get(theirs_start as usize..theirs_end as usize);
        if ours_changed && theirs_changed && differ {
            out.push(LineRange { start, end });
        }
        base_end = end;
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

    /// A chain of alternating overlaps (ours 2..4, theirs 3..6, ours 5..8,
    /// theirs 7..10, ours 9..11) is one conflict: clustering continues past
    /// its first pass until no hunk of either side overlaps it.
    #[test]
    fn a_chain_of_alternating_overlaps_is_one_conflict() {
        let base: String = (0..14).map(|i| format!("l{i}\n")).collect();
        let change = |lines: &[usize], tag: &str| -> String {
            (0..14)
                .map(|i| {
                    if lines.contains(&i) {
                        format!("{tag}{i}\n")
                    } else {
                        format!("l{i}\n")
                    }
                })
                .collect()
        };
        let ours = change(&[2, 3, 5, 6, 7, 9, 10], "o");
        let theirs = change(&[3, 4, 5, 7, 8, 9], "t");
        assert_eq!(
            merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 2, end: 11 }])
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

    /// An insertion of one side before a change of the other, at another
    /// line, does not overlap it: only the real conflict is named.
    #[test]
    fn an_insertion_elsewhere_is_not_part_of_a_conflict() {
        let ours = BASE
            .replace("package a\n", "package a\n// ours\n")
            .replace("return 3", "return 30");
        let theirs = BASE
            .replace("return 2", "return 20")
            .replace("return 3", "return 33");
        assert_eq!(
            merge(BASE.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 6, end: 7 }])
        );
    }

    /// One side changes a line and the other inserts right after it. The
    /// hunks touch (the insertion sits at the changed line's end), and the
    /// driver, as git does, merges touching hunks into one region that both
    /// sides changed: a conflict naming the changed line. A false conflict
    /// costs a writer one retry; a false clean merge would land bytes no
    /// side wrote together (ADR-0045 T5, amended 2026-11-22).
    #[test]
    fn an_insertion_after_a_line_the_other_side_changed_conflicts_on_that_line() {
        let base = "fn main() {}\n";
        let theirs = "fn main() { let _ = 1; }\n";
        let ours = "fn main() {}\n\nfn ours() {}\n";
        assert_eq!(
            merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 0, end: 1 }])
        );
        // The same collision with the sides swapped names the same lines.
        assert_eq!(
            merge(base.as_bytes(), theirs.as_bytes(), ours.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 0, end: 1 }])
        );
    }

    /// An insertion right before a changed line touches it too.
    #[test]
    fn an_insertion_before_a_line_the_other_side_changed_conflicts_on_that_line() {
        let ours = BASE.replace("func B", "// ours\nfunc B");
        let theirs = edit("return 2", "return 20");
        assert_eq!(
            merge(BASE.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict(vec![LineRange { start: 4, end: 5 }])
        );
    }

    /// Changes one unchanged line apart do not touch: they merge.
    #[test]
    fn changes_one_line_apart_merge_cleanly() {
        let base = "a\nb\nc\n";
        let ours = "A\nb\nc\n";
        let theirs = "a\nb\nC\n";
        assert_eq!(
            merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Clean(b"A\nb\nC\n".to_vec())
        );
    }

    /// Every file over a small alphabet, up to three lines (a side's last
    /// line with or without its newline): whenever the
    /// driver reports a conflict, [`merge`] names at least one base range,
    /// each inside the base and ordered; and the ranges are named only when
    /// the driver reports a conflict. An empty range is only ever two
    /// insertions at one line, which the merge cannot order.
    #[test]
    fn a_conflict_always_names_its_base_ranges() {
        use gix_merge::blob::{Resolution, builtin_driver::text};
        fn files(alphabet: &[&str]) -> Vec<String> {
            let mut all = vec![String::new()];
            let mut last = vec![String::new()];
            for _ in 0..3 {
                last = last
                    .iter()
                    .flat_map(|f| alphabet.iter().map(move |l| format!("{f}{l}\n")))
                    .collect();
                all.extend(last.iter().cloned());
            }
            all
        }
        let bases = files(&["a", "b"]);
        // Each side also without its last newline: a final line the driver
        // tokenizes unterminated.
        let mut sides = files(&["a", "b", "c"]);
        let unterminated: Vec<String> = sides
            .iter()
            .filter_map(|f| f.strip_suffix('\n').map(str::to_string))
            .collect();
        sides.extend(unterminated);
        let mut conflicts = 0usize;
        for base in &bases {
            let base_lines = u32::try_from(base.lines().count()).unwrap();
            for ours in &sides {
                for theirs in &sides {
                    let (base, ours, theirs) =
                        (base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
                    let mut out = Vec::new();
                    let mut input = imara_diff::InternedInput::default();
                    let driver = text(
                        &mut out,
                        &mut input,
                        text::Labels::default(),
                        ours,
                        base,
                        theirs,
                        text::Options::default(),
                    );
                    let ranges = conflict_ranges(base, ours, theirs);
                    let what = || {
                        format!(
                            "base {:?} ours {:?} theirs {:?}",
                            String::from_utf8_lossy(base),
                            String::from_utf8_lossy(ours),
                            String::from_utf8_lossy(theirs)
                        )
                    };
                    assert_eq!(
                        driver == Resolution::Conflict,
                        !ranges.is_empty(),
                        "driver and ranges disagree: {}",
                        what()
                    );
                    for pair in ranges.windows(2) {
                        assert!(pair[0].end < pair[1].start, "unordered: {}", what());
                    }
                    for r in &ranges {
                        assert!(
                            r.start <= r.end && r.end <= base_lines,
                            "outside base: {}",
                            what()
                        );
                    }
                    if let Merged::Conflict(named) = merge(base, ours, theirs) {
                        assert!(!named.is_empty(), "unnamed conflict: {}", what());
                        conflicts += 1;
                    }
                }
            }
        }
        assert!(conflicts > 0, "the enumeration reaches conflicts");
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

    fn range(start: u32, end: u32) -> LineRange {
        LineRange { start, end }
    }

    /// A replaced line, an inserted line and a deleted line, each reported
    /// with the lines of both sides it spans.
    #[test]
    fn changed_lines_reports_each_hunk_on_both_sides() {
        let from = "a\nb\nc\nd\n";
        assert_eq!(
            changed_lines(from.as_bytes(), "a\nB\nc\nd\n".as_bytes()),
            vec![Change {
                from: range(1, 2),
                to: range(1, 2)
            }]
        );
        assert_eq!(
            changed_lines(from.as_bytes(), "a\nb\nx\nc\nd\n".as_bytes()),
            vec![Change {
                from: range(2, 2),
                to: range(2, 3)
            }]
        );
        assert_eq!(
            changed_lines(from.as_bytes(), "a\nb\nd\n".as_bytes()),
            vec![Change {
                from: range(2, 3),
                to: range(2, 2)
            }]
        );
        assert_eq!(changed_lines(from.as_bytes(), from.as_bytes()), vec![]);
    }

    /// Lines are split on newlines whatever the bytes are.
    #[test]
    fn changed_lines_diffs_bytes_that_are_not_utf8() {
        assert_eq!(
            changed_lines(b"\xff\nb\n", b"\xff\nc\n"),
            vec![Change {
                from: range(1, 2),
                to: range(1, 2)
            }]
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
