//! A tiny, dependency-free line-level unified diff.
//!
//! Splits both sides into lines, aligns them, and groups the edits into unified-style
//! hunks with up to [`CONTEXT`] lines of surrounding context. Used by `keel diff` to show
//! what actually changed in a file between `HEAD` and the working tree.
//!
//! The alignment first trims the common prefix and suffix (most edits are local, so this
//! collapses a huge file with a small change to a tiny differing middle), then runs an LCS
//! dynamic program on just that middle. The LCS table is O(m·n), so a hard [`CAP`] guards
//! against a pathological whole-file rewrite (e.g. an 11k-line file with edits throughout):
//! past the cap the middle is emitted as a plain delete-all-then-add-all block, which is
//! correct if coarse. For localized edits — the common case — the trim makes it cheap.

/// Lines of unchanged context kept around each change in a hunk.
const CONTEXT: usize = 3;
/// Max LCS table cells (m·n) before falling back to a coarse replace block.
const CAP: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Context,
    Del,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub tag: Tag,
    pub text: String,
}

/// One unified-diff hunk. `*_start` are 1-based line numbers (git-style); a `*_len` of 0
/// carries a `*_start` of the line *after which* the change applies (so a pure insertion
/// into an empty side reads as `-0,0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

// An aligned step over the two line vectors. Indices point into `old` (Eq/Del) or `new` (Add).
enum Op {
    Eq(usize),
    Del(usize),
    Add(usize),
}

/// Unified diff of two texts, compared line by line (`str::lines`, so a trailing newline
/// does not produce a spurious empty line). Returns an empty `Vec` when `old == new`.
pub fn diff_lines(old: &str, new: &str) -> Vec<Hunk> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    // Trim common prefix / suffix — these become the hunks' context and keep the LCS small.
    let mut lo = 0;
    while lo < a.len() && lo < b.len() && a[lo] == b[lo] {
        lo += 1;
    }
    let (mut ha, mut hb) = (a.len(), b.len());
    while ha > lo && hb > lo && a[ha - 1] == b[hb - 1] {
        ha -= 1;
        hb -= 1;
    }

    // Full op list: prefix equals, the aligned middle, then suffix equals.
    let mut ops: Vec<Op> = Vec::new();
    for i in 0..lo {
        ops.push(Op::Eq(i));
    }
    align_middle(&a[lo..ha], &b[lo..hb], lo, lo, &mut ops);
    for k in 0..(a.len() - ha) {
        ops.push(Op::Eq(ha + k)); // suffix: a[ha+k] == b[hb+k]
    }

    hunkify(&a, &b, &ops)
}

/// LCS-align two middle slices (offsets map middle indices back to the full vectors),
/// pushing `Op`s in order. Falls back to delete-all-then-add-all past [`CAP`].
fn align_middle(a: &[&str], b: &[&str], off_a: usize, off_b: usize, out: &mut Vec<Op>) {
    let (m, n) = (a.len(), b.len());
    if m == 0 && n == 0 {
        return;
    }
    if m == 0 {
        (0..n).for_each(|j| out.push(Op::Add(off_b + j)));
        return;
    }
    if n == 0 {
        (0..m).for_each(|i| out.push(Op::Del(off_a + i)));
        return;
    }
    if m.saturating_mul(n) > CAP {
        // pathological middle — don't build a giant table; emit a coarse replacement
        (0..m).for_each(|i| out.push(Op::Del(off_a + i)));
        (0..n).for_each(|j| out.push(Op::Add(off_b + j)));
        return;
    }

    // dp[i][j] = LCS length of a[i..], b[j..]
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] =
                if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if a[i] == b[j] {
            out.push(Op::Eq(off_a + i));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(Op::Del(off_a + i));
            i += 1;
        } else {
            out.push(Op::Add(off_b + j));
            j += 1;
        }
    }
    (i..m).for_each(|i| out.push(Op::Del(off_a + i)));
    (j..n).for_each(|j| out.push(Op::Add(off_b + j)));
}

/// Group the ops into hunks: runs of changes, each padded with up to [`CONTEXT`] context
/// lines; runs closer than `2*CONTEXT` unchanged lines merge into one hunk.
fn hunkify(a: &[&str], b: &[&str], ops: &[Op]) -> Vec<Hunk> {
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, Op::Del(_) | Op::Add(_)))
        .map(|(k, _)| k)
        .collect();
    if changes.is_empty() {
        return Vec::new();
    }

    // running line-number prefixes: pre_old[k]/pre_new[k] = old/new lines in ops[0..k]
    let mut pre_old = vec![0usize; ops.len() + 1];
    let mut pre_new = vec![0usize; ops.len() + 1];
    for (k, op) in ops.iter().enumerate() {
        pre_old[k + 1] = pre_old[k] + usize::from(matches!(op, Op::Eq(_) | Op::Del(_)));
        pre_new[k + 1] = pre_new[k] + usize::from(matches!(op, Op::Eq(_) | Op::Add(_)));
    }

    // coalesce change positions into [first, last] op-index groups
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let (mut gstart, mut gprev) = (changes[0], changes[0]);
    for &p in &changes[1..] {
        if p - gprev - 1 > 2 * CONTEXT {
            groups.push((gstart, gprev));
            gstart = p;
        }
        gprev = p;
    }
    groups.push((gstart, gprev));

    groups
        .into_iter()
        .map(|(fc, lc)| {
            let s = fc.saturating_sub(CONTEXT);
            let e = (lc + CONTEXT + 1).min(ops.len());
            let old_len = pre_old[e] - pre_old[s];
            let new_len = pre_new[e] - pre_new[s];
            let lines = ops[s..e]
                .iter()
                .map(|op| match *op {
                    Op::Eq(i) => DiffLine { tag: Tag::Context, text: a[i].to_string() },
                    Op::Del(i) => DiffLine { tag: Tag::Del, text: a[i].to_string() },
                    Op::Add(j) => DiffLine { tag: Tag::Add, text: b[j].to_string() },
                })
                .collect();
            Hunk {
                old_start: if old_len > 0 { pre_old[s] + 1 } else { pre_old[s] },
                old_len,
                new_start: if new_len > 0 { pre_new[s] + 1 } else { pre_new[s] },
                new_len,
                lines,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(h: &Hunk) -> Vec<(Tag, &str)> {
        h.lines.iter().map(|l| (l.tag, l.text.as_str())).collect()
    }

    #[test]
    fn identical_inputs_produce_no_hunks() {
        assert!(diff_lines("a\nb\nc\n", "a\nb\nc\n").is_empty());
        assert!(diff_lines("", "").is_empty());
        // a trailing-newline-only difference is not a line difference under `str::lines`
        assert!(diff_lines("a\nb", "a\nb\n").is_empty());
    }

    #[test]
    fn all_added_into_empty_reads_as_0_0() {
        let h = diff_lines("", "x\ny\n");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_start, h[0].old_len), (0, 0), "empty old side → -0,0");
        assert_eq!((h[0].new_start, h[0].new_len), (1, 2));
        assert_eq!(tags(&h[0]), vec![(Tag::Add, "x"), (Tag::Add, "y")]);
    }

    #[test]
    fn all_removed_to_empty() {
        let h = diff_lines("x\ny\n", "");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_start, h[0].old_len), (1, 2));
        assert_eq!((h[0].new_start, h[0].new_len), (0, 0), "empty new side → +0,0");
        assert_eq!(tags(&h[0]), vec![(Tag::Del, "x"), (Tag::Del, "y")]);
    }

    #[test]
    fn single_line_change_carries_context() {
        let old = "l1\nl2\nl3\nl4\nl5\n";
        let new = "l1\nl2\nCHANGED\nl4\nl5\n";
        let h = diff_lines(old, new);
        assert_eq!(h.len(), 1);
        // 3 context (only 2 available above) + del + add + 2 context below
        assert_eq!(
            tags(&h[0]),
            vec![
                (Tag::Context, "l1"),
                (Tag::Context, "l2"),
                (Tag::Del, "l3"),
                (Tag::Add, "CHANGED"),
                (Tag::Context, "l4"),
                (Tag::Context, "l5"),
            ]
        );
        assert_eq!((h[0].old_start, h[0].old_len), (1, 5));
        assert_eq!((h[0].new_start, h[0].new_len), (1, 5));
    }

    #[test]
    fn two_far_apart_changes_make_two_hunks() {
        // 20 identical middle lines keep the two edits > 2*CONTEXT apart
        let mut old = String::from("HEAD\n");
        for i in 0..20 {
            old.push_str(&format!("m{i}\n"));
        }
        old.push_str("TAIL\n");
        let new = old.replace("HEAD", "HEAD2").replace("TAIL", "TAIL2");
        let h = diff_lines(&old, &new);
        assert_eq!(h.len(), 2, "distant edits are separate hunks");
    }

    #[test]
    fn two_near_changes_merge_into_one_hunk() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "A\nb\nc\nd\nE\n"; // edits 4 lines apart (< 2*CONTEXT) → merge
        let h = diff_lines(old, new);
        assert_eq!(h.len(), 1, "nearby edits coalesce");
    }

    #[test]
    fn handles_large_middle_without_exploding() {
        // ~5000 distinct lines each side, fully different → exceeds CAP, coarse fallback
        let old: String = (0..5000).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("new{i}\n")).collect();
        let h = diff_lines(&old, &new);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].old_len, 5000);
        assert_eq!(h[0].new_len, 5000);
    }
}
