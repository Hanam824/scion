//! Line-based three-way merge (diff3 style).

use crate::text::split_lines;
use similar::{capture_diff_slices, Algorithm, DiffOp};

#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(String),
    /// Text with conflict markers, for display only.
    Conflict(String),
}

/// A region of base lines `[start, end)` replaced by side lines `[new_start, new_end)`.
#[derive(Clone, Copy, Debug)]
struct Hunk {
    start: usize,
    end: usize,
    new_start: usize,
    new_end: usize,
}

fn hunks(base: &[&str], side: &[&str]) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, base, side) {
        let h = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => Hunk {
                start: old_index,
                end: old_index + old_len,
                new_start: new_index,
                new_end: new_index,
            },
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => Hunk {
                start: old_index,
                end: old_index,
                new_start: new_index,
                new_end: new_index + new_len,
            },
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => Hunk {
                start: old_index,
                end: old_index + old_len,
                new_start: new_index,
                new_end: new_index + new_len,
            },
        };
        // Coalesce adjacent edits (e.g. delete followed by insert).
        match out.last_mut() {
            Some(prev) if prev.end == h.start && prev.new_end == h.new_start => {
                prev.end = h.end;
                prev.new_end = h.new_end;
            }
            _ => out.push(h),
        }
    }
    out
}

/// One side's version of base lines `[lo, hi)`, given its hunks in that range.
fn side_text(base: &[&str], side: &[&str], group: &[Hunk], lo: usize, hi: usize) -> String {
    let mut s = String::new();
    let mut p = lo;
    for h in group {
        s.push_str(&base[p..h.start].concat());
        s.push_str(&side[h.new_start..h.new_end].concat());
        p = h.end;
    }
    s.push_str(&base[p..hi].concat());
    s
}

/// Merges `ours` and `theirs`, both derived from `base`. Edits that overlap or
/// touch conflict unless both sides made the identical edit.
pub fn merge_text(base: &str, ours: &str, theirs: &str, labels: (&str, &str)) -> MergeOutcome {
    let b = split_lines(base);
    let o = split_lines(ours);
    let t = split_lines(theirs);
    let ho = hunks(&b, &o);
    let ht = hunks(&b, &t);

    let mut out = String::new();
    let mut conflict = false;
    let (mut i, mut j, mut pos) = (0, 0, 0);
    while i < ho.len() || j < ht.len() {
        let (gi, gj) = (i, j);
        let (lo, mut hi);
        if j >= ht.len() || (i < ho.len() && ho[i].start <= ht[j].start) {
            lo = ho[i].start;
            hi = ho[i].end;
            i += 1;
        } else {
            lo = ht[j].start;
            hi = ht[j].end;
            j += 1;
        }
        loop {
            let mut grew = false;
            if i < ho.len() && ho[i].start <= hi {
                hi = hi.max(ho[i].end);
                i += 1;
                grew = true;
            }
            if j < ht.len() && ht[j].start <= hi {
                hi = hi.max(ht[j].end);
                j += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }
        out.push_str(&b[pos..lo].concat());
        let (og, tg) = (&ho[gi..i], &ht[gj..j]);
        if tg.is_empty() {
            out.push_str(&side_text(&b, &o, og, lo, hi));
        } else if og.is_empty() {
            out.push_str(&side_text(&b, &t, tg, lo, hi));
        } else {
            let a = side_text(&b, &o, og, lo, hi);
            let c = side_text(&b, &t, tg, lo, hi);
            if a == c {
                out.push_str(&a);
            } else {
                conflict = true;
                out.push_str(&format!("<<<<<<< {}\n", labels.0));
                push_line_block(&mut out, &a);
                out.push_str("=======\n");
                push_line_block(&mut out, &c);
                out.push_str(&format!(">>>>>>> {}\n", labels.1));
            }
        }
        pos = hi;
    }
    out.push_str(&b[pos..].concat());
    if conflict {
        MergeOutcome::Conflict(out)
    } else {
        MergeOutcome::Clean(out)
    }
}

fn push_line_block(out: &mut String, s: &str) {
    out.push_str(s);
    if !s.is_empty() && !s.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: (&str, &str) = ("ours", "theirs");

    #[test]
    fn disjoint_edits_merge() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\n";
        assert_eq!(
            merge_text(base, ours, theirs, L),
            MergeOutcome::Clean("A\nb\nc\nd\nE\n".into())
        );
    }

    #[test]
    fn identical_edits_merge() {
        let base = "a\nb\n";
        let both = "a\nB\n";
        assert_eq!(
            merge_text(base, both, both, L),
            MergeOutcome::Clean(both.into())
        );
    }

    #[test]
    fn overlapping_edits_conflict() {
        let base = "a\nb\nc\n";
        match merge_text(base, "a\nX\nc\n", "a\nY\nc\n", L) {
            MergeOutcome::Conflict(t) => {
                assert_eq!(t, "a\n<<<<<<< ours\nX\n=======\nY\n>>>>>>> theirs\nc\n")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn insertions_and_deletions() {
        let base = "1\n2\n3\n4\n5\n6\n";
        let ours = "0\n1\n2\n3\n4\n5\n6\n";
        let theirs = "1\n2\n3\n4\n6\n";
        assert_eq!(
            merge_text(base, ours, theirs, L),
            MergeOutcome::Clean("0\n1\n2\n3\n4\n6\n".into())
        );
    }

    #[test]
    fn one_side_only() {
        let base = "x\n";
        assert_eq!(
            merge_text(base, base, "y\n", L),
            MergeOutcome::Clean("y\n".into())
        );
        assert_eq!(
            merge_text("", "", "new\n", L),
            MergeOutcome::Clean("new\n".into())
        );
    }
}
