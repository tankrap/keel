//! Semantic diff, levels 0–1 of the depth ladder (see `src/docs/semantic-depth-and-decentralization.md`).
//!
//! A line diff answers "what text changed". A reviewer wants "what *operation* happened" — one
//! rename across 15 files, not 15 line changes to read. This module collapses the mechanical bulk and
//! surfaces what actually needs eyes, deterministically and with no model in the loop:
//!
//! - **Level 0 — lexical mask + frequency grouping.** Each changed line is masked to a *shape*
//!   (identifiers → `_`, numbers → `#`, strings → `""`, whitespace collapsed, operators kept). Lines
//!   sharing a shape are grouped; a shape recurring `MECHANICAL_MIN`+ times is *mechanical* (a bulk
//!   edit — renames, codemods, formatting), the rest are *substantive* (unique changes to read).
//! - **Level 1 — literal-anomaly split.** Masking throws numbers away, which is exactly where a bug
//!   can hide (a `* 2` → `* 3` smuggled into a rename). Within a mechanical group we keep each site's
//!   concrete numeric literals and surface a *minority* value at a position an overwhelming majority
//!   agrees on — so the smuggled constant goes from HIDDEN to SURFACED, while a uniformly-varying
//!   position (`i = 0`, `1`, … `19` across 20 sites) stays compressed with no false anomaly.
//!
//! The unit is the **added line** (the new state a reviewer scrutinizes). Deeper levels (AST,
//! dataflow, cross-file) are roadmapped and need the hosted, memoized graph.

use std::collections::BTreeMap;

/// A shape must recur at least this many times to count as a mechanical (bulk) edit.
pub const MECHANICAL_MIN: usize = 3;

/// Whether a group of same-shaped lines is bulk (collapsible) or unique (needs review).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupKind {
    Mechanical,
    Substantive,
}

/// One added line, tagged with the file it came from — the unit the engine works on, so an anomaly
/// or a substantive change can name where it lives (essential once a change spans many files).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AddedLine {
    pub file: String,
    pub text: String,
}

impl AddedLine {
    pub fn new(file: impl Into<String>, text: impl Into<String>) -> Self {
        AddedLine { file: file.into(), text: text.into() }
    }
}

/// One site whose literal vector is an outlier within its mechanical group.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Anomaly {
    /// The file the anomalous line is in.
    pub file: String,
    /// The added line text.
    pub text: String,
    /// Why it was flagged, e.g. `literal 3 where 15/16 use 2`.
    pub reason: String,
}

/// A set of added lines sharing one masked shape.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangeGroup {
    pub shape: String,
    pub kind: GroupKind,
    /// Every added line in this group (file + text), in first-seen order.
    pub members: Vec<AddedLine>,
    /// Literal-anomaly sites (mechanical groups only).
    pub anomalies: Vec<Anomaly>,
}

impl ChangeGroup {
    pub fn count(&self) -> usize {
        self.members.len()
    }
    /// A concrete example to show for a collapsed group — the first non-anomalous member if any
    /// (so the example reads as the "normal" case), else the first member.
    pub fn representative(&self) -> &AddedLine {
        let anomalous: std::collections::HashSet<(&str, &str)> =
            self.anomalies.iter().map(|a| (a.file.as_str(), a.text.as_str())).collect();
        self.members
            .iter()
            .find(|m| !anomalous.contains(&(m.file.as_str(), m.text.as_str())))
            .or_else(|| self.members.first())
            .unwrap_or(&EMPTY_LINE)
    }
}

/// Fallback for `representative()` on the impossible empty-group case (a group always has ≥1 member).
static EMPTY_LINE: AddedLine = AddedLine { file: String::new(), text: String::new() };

/// The whole added-line side of a diff, grouped. Substantive groups come first (they need review),
/// then mechanical groups by descending size (the biggest bulk edits).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SemanticSummary {
    pub groups: Vec<ChangeGroup>,
    pub added_lines: usize,
    pub mechanical_lines: usize,
    pub substantive_lines: usize,
    pub anomaly_count: usize,
}

/// Mask a line to its structural shape: identifiers → `_`, numbers → `#`, string literals → `""`,
/// runs of whitespace collapsed to one space (and trimmed), operators/punctuation kept verbatim.
pub fn mask_shape(line: &str) -> String {
    let (shape, _) = mask_and_literals(line);
    shape
}

/// A literal recovered from what the mask erases: a number (behind `#`) or a string body (behind
/// `""`). Both are Level-1 anomaly material — a smuggled `* 3` and a smuggled `"v3"` are the same bug.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Lit {
    is_string: bool,
    value: String,
}

impl Lit {
    /// How the value reads in an anomaly reason: strings are re-quoted, numbers bare.
    fn show(&self) -> String {
        if self.is_string {
            format!("{:?}", self.value)
        } else {
            self.value.clone()
        }
    }
}

/// The numeric literals in a line, in order (the Level-1 vector recovered from what the mask erases).
pub fn numeric_literals(line: &str) -> Vec<String> {
    mask_and_literals(line).1.into_iter().filter(|l| !l.is_string).map(|l| l.value).collect()
}

/// Single pass producing both the shape and the literal vector (numbers and string bodies, in order).
fn mask_and_literals(line: &str) -> (String, Vec<Lit>) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut out = String::new();
    let mut lits = Vec::new();
    let mut pending_space = false; // collapse whitespace, emit lazily so we never leave a trailing one
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            i += 1;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if c == '_' || c.is_alphabetic() {
            // identifier / keyword → `_`
            while i < chars.len() && (chars[i] == '_' || chars[i].is_alphanumeric()) {
                i += 1;
            }
            out.push('_');
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            // numeric literal (int / float / hex-ish) → `#`, value recorded
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            lits.push(Lit { is_string: false, value: chars[start..i].iter().collect() });
            out.push('#');
        } else if c == '"' && chars[i + 1..].contains(&'"') {
            // A *balanced* double-quoted string → `""`, body skipped (honoring `\` escapes). The
            // balance check matters: an UNbalanced `"` (a quote inside a comment, a mismatched
            // delimiter) must NOT be treated as a string, or it would swallow the rest of the line and
            // drop the numeric literals Level 1 depends on. Single quotes are never string delimiters
            // here — in the languages keel targets a lone `'` is far more often a Rust lifetime
            // (`&'a`), an apostrophe (`// don't`), or a char literal than the start of a string — so
            // treating it as a plain char keeps `... = f(2)` vs `... = f(3)` literals intact.
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            // Capture the body (escapes verbatim, so comparison is exact) — a smuggled string constant
            // is as much an anomaly as a smuggled number.
            lits.push(Lit { is_string: true, value: chars[start..i.min(chars.len())].iter().collect() });
            i += 1; // closing quote
            out.push('"');
            out.push('"');
        } else {
            out.push(c);
            i += 1;
        }
    }
    (out, lits)
}

/// Group the added lines of a diff into a [`SemanticSummary`].
pub fn summarize(added: &[AddedLine]) -> SemanticSummary {
    // Preserve first-seen order of shapes for stable output.
    let mut order: Vec<String> = Vec::new();
    let mut by_shape: BTreeMap<String, Vec<(AddedLine, Vec<Lit>)>> = BTreeMap::new();
    for line in added {
        let (shape, lits) = mask_and_literals(&line.text);
        if !by_shape.contains_key(&shape) {
            order.push(shape.clone());
        }
        by_shape.entry(shape).or_default().push((line.clone(), lits));
    }

    let mut groups = Vec::new();
    let (mut mech_lines, mut subst_lines, mut anomaly_count) = (0usize, 0usize, 0usize);
    for shape in &order {
        let sites = &by_shape[shape];
        let kind = if sites.len() >= MECHANICAL_MIN { GroupKind::Mechanical } else { GroupKind::Substantive };
        let members: Vec<AddedLine> = sites.iter().map(|(l, _)| l.clone()).collect();
        let anomalies = if kind == GroupKind::Mechanical {
            detect_anomalies(sites)
        } else {
            Vec::new()
        };
        match kind {
            GroupKind::Mechanical => mech_lines += members.len(),
            GroupKind::Substantive => subst_lines += members.len(),
        }
        anomaly_count += anomalies.len();
        groups.push(ChangeGroup { shape: shape.clone(), kind, members, anomalies });
    }

    // Substantive first (needs review), then mechanical by descending size. Stable within each band.
    groups.sort_by(|a, b| match (a.kind, b.kind) {
        (GroupKind::Substantive, GroupKind::Mechanical) => std::cmp::Ordering::Less,
        (GroupKind::Mechanical, GroupKind::Substantive) => std::cmp::Ordering::Greater,
        (GroupKind::Mechanical, GroupKind::Mechanical) => b.count().cmp(&a.count()),
        (GroupKind::Substantive, GroupKind::Substantive) => std::cmp::Ordering::Equal,
    });

    SemanticSummary {
        groups,
        added_lines: added.len(),
        mechanical_lines: mech_lines,
        substantive_lines: subst_lines,
        anomaly_count,
    }
}

/// The mode at a literal position must cover at least this fraction of sites (an *overwhelming*
/// supermajority) before a differing site is called an anomaly. A bare 50%+1 is not enough: at ~50/50
/// (or a 2-vs-1 split at the [`MECHANICAL_MIN`] floor) the "minority" is just ordinary variation, not
/// a smuggled outlier. Expressed as integers below (`mode_count * 5 >= n * 4`) to avoid floats.
const ANOMALY_SUPERMAJORITY_NUM: usize = 4;
const ANOMALY_SUPERMAJORITY_DEN: usize = 5;

/// Level 1: within one mechanical group, flag a site whose literal (number *or* string body) at some
/// position differs from a value an *overwhelming supermajority* (≥ 80%, see
/// [`ANOMALY_SUPERMAJORITY_NUM`]) of sites share. A position where values are spread out (uniform
/// variation like `i = 0..19`) has no such dominant value and flags nothing, and neither does a
/// near-even split.
fn detect_anomalies(sites: &[(AddedLine, Vec<Lit>)]) -> Vec<Anomaly> {
    let n = sites.len();
    // All sites share a shape ⇒ the same sequence of `#`/`""` placeholders ⇒ equal-length literal
    // vectors with the same kind at each position.
    let width = sites.first().map(|(_, l)| l.len()).unwrap_or(0);
    if width == 0 {
        return Vec::new(); // no literals to compare
    }

    // reasons[site_index] accumulates the anomalous positions for that site.
    let mut reasons: Vec<Vec<String>> = vec![Vec::new(); n];
    for p in 0..width {
        // Tally values at position p (by string value; kind is constant across sites at a position).
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, lits) in sites {
            if let Some(v) = lits.get(p) {
                *counts.entry(v.value.as_str()).or_default() += 1;
            }
        }
        let Some((mode, &mode_count)) = counts.iter().max_by_key(|(_, &c)| c).map(|(m, c)| (*m, c))
        else {
            continue;
        };
        // Only an overwhelming supermajority (≥ 80%) makes a minority "anomalous"; a near-even split
        // or high-cardinality position is expected variation, not a smuggled outlier.
        if mode_count * ANOMALY_SUPERMAJORITY_DEN < n * ANOMALY_SUPERMAJORITY_NUM {
            continue;
        }
        for (idx, (_, lits)) in sites.iter().enumerate() {
            if let Some(v) = lits.get(p) {
                if v.value != mode {
                    let mode_show = if v.is_string { format!("{mode:?}") } else { mode.to_string() };
                    reasons[idx].push(format!(
                        "literal {} where {mode_count}/{n} use {mode_show}",
                        v.show()
                    ));
                }
            }
        }
    }

    let mut out = Vec::new();
    for (idx, rs) in reasons.into_iter().enumerate() {
        if !rs.is_empty() {
            let site = &sites[idx].0;
            out.push(Anomaly { file: site.file.clone(), text: site.text.clone(), reason: rs.join("; ") });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap plain text lines as [`AddedLine`]s from a single dummy file (most tests don't care which).
    fn al(texts: Vec<String>) -> Vec<AddedLine> {
        texts.into_iter().map(|t| AddedLine::new("f.rs", t)).collect()
    }

    #[test]
    fn mask_normalizes_identifiers_numbers_strings_and_whitespace() {
        // leading + doubled whitespace collapses; a space that IS between tokens is preserved
        assert_eq!(mask_shape("  let  total = subTotal * 2 ;"), "_ _ = _ * # ;");
        assert_eq!(mask_shape("foo(bar, 42, \"hi\")"), "_(_, #, \"\")");
        assert_eq!(mask_shape("x >= 0 && y != 1.5"), "_ >= # && _ != #");
        // identifiers with digits stay one identifier; a leading-dot float is a number
        assert_eq!(mask_shape("v2 = .5"), "_ = #");
    }

    #[test]
    fn numeric_literals_are_recovered_in_order() {
        assert_eq!(numeric_literals("a = b * 2 + 3"), vec!["2", "3"]);
        assert_eq!(numeric_literals("rename(item, thing)"), Vec::<String>::new());
    }

    #[test]
    fn frequency_grouping_splits_mechanical_from_substantive() {
        // three identical-shaped renames (mechanical) + one unique line (substantive)
        let added = vec![
            "renderRow(a, 0)".to_string(),
            "renderRow(b, 0)".to_string(),
            "renderRow(c, 0)".to_string(),
            "let total = subtotal * taxRate + shipping".to_string(),
        ];
        let s = summarize(&al(added));
        assert_eq!(s.mechanical_lines, 3);
        assert_eq!(s.substantive_lines, 1);
        // substantive sorts first
        assert_eq!(s.groups[0].kind, GroupKind::Substantive);
        assert_eq!(s.groups[1].kind, GroupKind::Mechanical);
        assert_eq!(s.groups[1].count(), 3);
    }

    #[test]
    fn literal_anomaly_surfaces_a_smuggled_constant() {
        // 15 sites multiply by 2, one smuggles in a 3 — same shape, minority literal ⇒ SURFACED
        let mut added: Vec<String> = (0..15).map(|i| format!("out{i} = in{i} * 2")).collect();
        added.push("outX = inX * 3".to_string());
        let s = summarize(&al(added));
        assert_eq!(s.groups.len(), 1, "all one shape");
        let g = &s.groups[0];
        assert_eq!(g.kind, GroupKind::Mechanical);
        assert_eq!(g.anomalies.len(), 1, "exactly the smuggled site");
        assert_eq!(g.anomalies[0].text, "outX = inX * 3");
        assert!(g.anomalies[0].reason.contains("literal 3 where 15/16 use 2"), "{}", g.anomalies[0].reason);
        // the representative is a normal (non-anomalous) site
        assert!(g.representative().text.ends_with("* 2"));
    }

    #[test]
    fn uniform_variation_is_not_a_false_anomaly() {
        // 20 loop headers whose only literal counts 0..19 — no majority value, so nothing is flagged
        let added: Vec<String> =
            (0..20).map(|i| format!("for (k = {i}; k < n; k++)")).collect();
        let s = summarize(&al(added));
        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.groups[0].kind, GroupKind::Mechanical);
        assert_eq!(s.groups[0].anomalies.len(), 0, "uniform variation must not flag");
        assert_eq!(s.anomaly_count, 0);
    }

    #[test]
    fn mixed_positions_flag_only_the_constant_one() {
        // position 0 varies uniformly (index), position 1 is constant 8 except one 9 ⇒ only the 9 flags
        let mut added: Vec<String> = (0..10).map(|i| format!("buf[{i}] = read(8)")).collect();
        added.push("buf[99] = read(9)".to_string());
        let s = summarize(&al(added));
        let g = &s.groups[0];
        assert_eq!(g.anomalies.len(), 1);
        assert_eq!(g.anomalies[0].text, "buf[99] = read(9)");
        assert!(g.anomalies[0].reason.contains("literal 9 where 10/11 use 8"));
    }

    #[test]
    fn lifetimes_and_apostrophes_do_not_swallow_later_literals() {
        // A Rust lifetime must not start a "string" that eats the trailing literal — the exact
        // smuggled-constant bug this feature targets, in the Rust codebase keel itself is.
        let mut added: Vec<String> = (0..8).map(|i| format!("let r{i}: &'a T = scale(2)")).collect();
        added.push("let rX: &'a T = scale(3)".to_string());
        let s = summarize(&al(added));
        assert_eq!(s.groups.len(), 1, "one shape despite the lifetime");
        assert_eq!(s.groups[0].anomalies.len(), 1, "smuggled 3 still surfaced");
        assert!(s.groups[0].anomalies[0].text.ends_with("scale(3)"));
        // an apostrophe in prose/comment must not drop the literal after it
        assert_eq!(numeric_literals("// don't scale by 2"), vec!["2"]);
        // a BALANCED double-quoted string is still a string: its inner number is not a literal
        assert_eq!(numeric_literals(r#"log("scaled by 2")"#), Vec::<String>::new());
        assert_eq!(mask_shape(r#"f("hi")"#), "_(\"\")");
        // an UNbalanced double quote (comment) must not swallow the rest — literal survives
        assert_eq!(numeric_literals("say \"hi and 2"), vec!["2"]);
    }

    #[test]
    fn string_literal_anomaly_surfaces_a_smuggled_constant() {
        // a flag/version codemod: 15 sites set "v2", one botched to "v3" — same bug class as numbers,
        // now caught because string bodies are recovered too
        let mut added: Vec<String> = (0..15).map(|i| format!("cfg{i}.set(\"v2\")")).collect();
        added.push("cfgX.set(\"v3\")".to_string());
        let s = summarize(&al(added));
        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.groups[0].anomalies.len(), 1);
        assert_eq!(s.groups[0].anomalies[0].text, "cfgX.set(\"v3\")");
        // the reason re-quotes the string values
        assert!(
            s.groups[0].anomalies[0].reason.contains("literal \"v3\" where 15/16 use \"v2\""),
            "{}",
            s.groups[0].anomalies[0].reason
        );
        // uniform string variation (each site a distinct label) must NOT flag
        let labels: Vec<String> = (0..20).map(|i| format!("register(\"evt{i}\")")).collect();
        assert_eq!(summarize(&al(labels)).groups[0].anomalies.len(), 0);
    }

    #[test]
    fn near_even_and_small_splits_are_not_flagged() {
        // 3-vs-2 at N=5 (60%) is ordinary variation, not a smuggled outlier
        let mut added: Vec<String> = (0..3).map(|i| format!("a{i} = f(2)")).collect();
        added.extend((0..2).map(|i| format!("b{i} = f(7)")));
        let s = summarize(&al(added));
        assert_eq!(s.groups[0].kind, GroupKind::Mechanical);
        assert_eq!(s.groups[0].anomalies.len(), 0, "60% is not an overwhelming majority");
        // 2-vs-1 at the MECHANICAL_MIN floor (N=3, 67%) must not flag either
        let added2 = vec!["p = g(4)".to_string(), "q = g(4)".to_string(), "r = g(9)".to_string()];
        assert_eq!(summarize(&al(added2)).groups[0].anomalies.len(), 0);
    }

    #[test]
    fn anomalies_and_members_carry_their_file() {
        // same shape across two files; the smuggled 3 lives in payments/tax.rs — the summary must say so
        let mut added: Vec<AddedLine> =
            (0..8).map(|i| AddedLine::new("ui/row.rs", format!("r{i} = scale(item{i}, 2)"))).collect();
        added.push(AddedLine::new("payments/tax.rs", "rX = scale(itemX, 3)".to_string()));
        let s = summarize(&added);
        let g = &s.groups[0];
        assert_eq!(g.anomalies.len(), 1);
        assert_eq!(g.anomalies[0].file, "payments/tax.rs", "anomaly names its file");
        assert_eq!(g.anomalies[0].text, "rX = scale(itemX, 3)");
        // the representative (a normal site) carries its file too
        assert_eq!(g.representative().file, "ui/row.rs");
        assert!(g.members.iter().any(|m| m.file == "payments/tax.rs"));
    }

    #[test]
    fn empty_and_no_literal_groups_are_safe() {
        assert_eq!(summarize(&Vec::<AddedLine>::new()).groups.len(), 0);
        // a mechanical group with no numeric literals → no anomalies, no panic
        let added = vec!["a.b()".to_string(), "c.d()".to_string(), "e.f()".to_string()];
        let s = summarize(&al(added));
        assert_eq!(s.groups[0].kind, GroupKind::Mechanical);
        assert_eq!(s.groups[0].anomalies.len(), 0);
    }
}
