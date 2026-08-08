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
//! - **Level 1b — operator-anomaly split.** The Level-0 mask keeps operators *verbatim*, so a flipped
//!   comparison lands in a different shape group and hides in plain sight (9 sites `if (i <= n)`, one
//!   `if (i < n)`). A second, *operator-agnostic* mask collapses the flip-prone operators — comparison
//!   and logical (`<`, `<=`, `==`, `&&`, `||`, `!`, …) — to a placeholder so those lines regroup; within
//!   a group we recover each site's operator vector and surface a *minority* operator the same 80%-
//!   supermajority way — the `<=`→`<` off-by-one, the `&&`→`||` guard inversion smuggled into a bulk
//!   edit. Structural tokens (a bare `=`, a `=>`/`->`/`<-` digraph, arithmetic/bitwise ops, brackets)
//!   stay verbatim, so assignments never regroup with comparisons and genuinely-varied operators (no
//!   80% agreement) never fire. Arithmetic/bitwise flips are excluded on purpose (they vary legitimately
//!   line-to-line — noise, not signal); see [`is_flip_op`].
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

/// One added line, tagged with where it came from — the file, and (best-effort) the enclosing symbol
/// (`fn compute_tax`, `class Foo`). The unit the engine works on, so an anomaly or substantive change
/// can name its location (essential once a change spans many files or long functions).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AddedLine {
    pub file: String,
    pub text: String,
    /// The enclosing definition, if the caller could determine one (e.g. `fn compute_tax`). `None`
    /// when top-level or undeterminable — never guessed, so it's a hint, never misleading.
    pub symbol: Option<String>,
}

impl AddedLine {
    /// A line with no symbol attribution (the caller sets `.symbol` directly when it has file context).
    pub fn new(file: impl Into<String>, text: impl Into<String>) -> Self {
        AddedLine { file: file.into(), text: text.into(), symbol: None }
    }
}

/// One site whose literal vector is an outlier within its mechanical group.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Anomaly {
    /// The file the anomalous line is in.
    pub file: String,
    /// The enclosing symbol, if known (carried from the anomalous line).
    pub symbol: Option<String>,
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
static EMPTY_LINE: AddedLine = AddedLine { file: String::new(), text: String::new(), symbol: None };

/// The whole added-line side of a diff, grouped. Substantive groups come first (they need review),
/// then mechanical groups by descending size (the biggest bulk edits).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SemanticSummary {
    pub groups: Vec<ChangeGroup>,
    /// Operator-anomaly sites (Level 1b) — a minority flipped operator (`<=`→`<`, `&&`→`||`) within an
    /// operator-agnostic group. Kept separate from the per-group literal anomalies because such a group
    /// spans several Level-0 shape groups (the flipped line is its own tiny shape), so it has no single
    /// home group.
    pub operator_anomalies: Vec<Anomaly>,
    pub added_lines: usize,
    pub mechanical_lines: usize,
    pub substantive_lines: usize,
    /// Total anomalies surfaced: per-group literal anomalies plus [`Self::operator_anomalies`].
    pub anomaly_count: usize,
}

impl SemanticSummary {
    /// Every anomaly surfaced by the diff — the per-group literal anomalies followed by the
    /// operator anomalies — for callers that render or record them uniformly (all four fields of an
    /// [`Anomaly`] stand alone; only the review-finding write-up distinguishes the two by label).
    pub fn all_anomalies(&self) -> impl Iterator<Item = &Anomaly> {
        self.groups.iter().flat_map(|g| g.anomalies.iter()).chain(self.operator_anomalies.iter())
    }
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

/// One lexical token of a line. The single source of truth for how a line is split — both masks
/// ([`mask_and_literals`] and [`mask_and_operators`]) fold this same stream, so identifier / number /
/// string / operator boundaries can never drift between the two.
enum Tok {
    Ident,       // identifier or keyword run
    Num(String), // numeric literal (int / float / hex-ish), value carried
    Str(String), // balanced double-quoted string, body carried
    Op(String),  // maximal run of operator chars (`<=`, `&&`, `=>`, `+`, `->`, …)
    Punct(char), // any other single non-space char (`(`, `,`, `;`, `.`, `:`, …)
    Space,       // a run of whitespace (collapsed at fold time)
}

/// Characters that combine into a single operator token (a maximal run). `=` is included so `==` /
/// `<=` / `+=` tokenize as one unit — but a bare `=` (assignment) is *structural*, not a flip (see
/// [`is_flip_op`]). `.` is excluded (member access / the point in a float, handled by the number scan),
/// as are `? : @ #` and brackets, so they stay verbatim and keep their shape-distinguishing role.
fn is_op_char(c: char) -> bool {
    matches!(c, '<' | '>' | '=' | '!' | '&' | '|' | '^' | '~' | '+' | '-' | '*' | '/' | '%')
}

/// Whether an operator token is *flip-prone* — a comparison (`<`,`<=`,`==`,…) or logical (`&&`,`||`,`!`)
/// operator, where a one-character *substitution* silently inverts the meaning (`<=`→`<` off-by-one,
/// `&&`→`||` guard inversion). These are the high-signal, low-noise flips: parallel bound checks or
/// guards rarely differ by design, so a lone substituted outlier is worth a look. (Note: this catches a
/// *substituted* operator, not a *dropped* one — a line missing its operator has a different op-shape,
/// so it never joins the sibling group to be flagged. Removing a whole guard is caught the other way,
/// as a removed-line anomaly.)
///
/// Deliberately *excluded*: arithmetic (`+ - * / %`), bitwise (`& | ^ ~ << >>`), and compound
/// assignment. Those legitimately vary line-to-line in parallel-looking code (offsets, deltas,
/// bitmasks), so flagging a minority there is noise, not signal — precision matters more than recall
/// for a review tool (a detector that cries wolf gets ignored). A run that isn't a flip — a bare `=`,
/// a `=>`/`->`/`<-` digraph, any arithmetic/bitwise op — is structural: kept verbatim in the operator
/// mask so it still distinguishes shapes but is never grouped-across or flagged. Arithmetic/bitwise
/// flips are roadmapped once a lower-noise heuristic (e.g. same-operand-shape) proves out.
fn is_flip_op(s: &str) -> bool {
    matches!(s, "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||" | "!")
}

/// Split a line into [`Tok`]s. Mirrors the original single-pass masker exactly for identifiers,
/// numbers (incl. leading-dot floats), and *balanced* double-quoted strings; additionally coalesces
/// runs of operator chars into one [`Tok::Op`] (so `<=` is one token, not `<` then `=`).
fn scan(line: &str) -> Vec<Tok> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            out.push(Tok::Space);
        } else if c == '_' || c.is_alphabetic() {
            while i < chars.len() && (chars[i] == '_' || chars[i].is_alphanumeric()) {
                i += 1;
            }
            out.push(Tok::Ident);
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Num(chars[start..i].iter().collect()));
        } else if c == '"' && chars[i + 1..].contains(&'"') {
            // A *balanced* double-quoted string → body carried (honoring `\` escapes). The balance
            // check matters: an UNbalanced `"` (a quote inside a comment, a mismatched delimiter) must
            // NOT be treated as a string, or it would swallow the rest of the line and drop the literals
            // Level 1 depends on. Single quotes are never string delimiters here — in the languages keel
            // targets a lone `'` is far more often a Rust lifetime (`&'a`), an apostrophe (`// don't`),
            // or a char literal than the start of a string, so it stays a plain char.
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            out.push(Tok::Str(chars[start..i.min(chars.len())].iter().collect()));
            i += 1; // closing quote
        } else if is_op_char(c) {
            let start = i;
            while i < chars.len() && is_op_char(chars[i]) {
                i += 1;
            }
            out.push(Tok::Op(chars[start..i].iter().collect()));
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

/// Fold a token stream into a masked shape, optionally masking flip-operators. With `mask_ops = false`
/// operators are kept verbatim — the Level-0 shape (identifiers → `_`, numbers → `#`, strings → `""`,
/// whitespace collapsed). With `mask_ops = true` each *flip* operator becomes `∘` and is recorded in
/// order (the Level-1b operator vector); structural operators still render verbatim. Returns the shape,
/// the literal vector, and the operator vector (the latter empty unless `mask_ops`).
fn render(toks: &[Tok], mask_ops: bool) -> (String, Vec<Lit>, Vec<String>) {
    let mut out = String::new();
    let mut lits = Vec::new();
    let mut ops = Vec::new();
    let mut pending_space = false; // collapse whitespace, emit lazily so we never leave a trailing one
    for t in toks {
        if matches!(t, Tok::Space) {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        match t {
            Tok::Ident => out.push('_'),
            Tok::Num(v) => {
                lits.push(Lit { is_string: false, value: v.clone() });
                out.push('#');
            }
            Tok::Str(v) => {
                lits.push(Lit { is_string: true, value: v.clone() });
                out.push_str("\"\"");
            }
            Tok::Op(s) => {
                if mask_ops && is_flip_op(s) {
                    ops.push(s.clone());
                    out.push('∘');
                } else {
                    out.push_str(s);
                }
            }
            Tok::Punct(c) => out.push(*c),
            Tok::Space => unreachable!("handled above"),
        }
    }
    (out, lits, ops)
}

/// Single pass producing both the Level-0 shape and the literal vector (numbers and string bodies).
fn mask_and_literals(line: &str) -> (String, Vec<Lit>) {
    let (shape, lits, _) = render(&scan(line), false);
    (shape, lits)
}

/// The operator-agnostic shape (flip-operators → `∘`) and the operator vector recovered from it, in
/// order. Two lines with the same op-shape have `∘`s at the same positions ⇒ equal-length, positionally
/// aligned operator vectors — what [`detect_operator_anomalies`] compares.
fn mask_and_operators(line: &str) -> (String, Vec<String>) {
    let (shape, _, ops) = render(&scan(line), true);
    (shape, ops)
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

    // Level 1b runs over the whole added-line set on its own (operator-agnostic) grouping, which cuts
    // across the Level-0 shape groups above.
    let operator_anomalies = detect_operator_anomalies(added);
    anomaly_count += operator_anomalies.len();

    SemanticSummary {
        groups,
        operator_anomalies,
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
            out.push(Anomaly {
                file: site.file.clone(),
                symbol: site.symbol.clone(),
                text: site.text.clone(),
                reason: rs.join("; "),
            });
        }
    }
    out
}

/// Level 1b: group added lines by *operator-agnostic* shape (flip-operators masked), then within each
/// group of ≥ [`MECHANICAL_MIN`] flag a site whose operator at some position differs from one an
/// overwhelming supermajority (≥ 80%, [`ANOMALY_SUPERMAJORITY_NUM`]) shares — the flipped `<`/`||`/`-`
/// smuggled into a bulk edit. Lines with no flip-operators are ignored (nothing to compare); a group
/// whose operators genuinely vary has no dominant value at any position and flags nothing. The 80%
/// gate means a group needs ≥ 5 sites before any minority can be called anomalous, exactly as Level 1.
fn detect_operator_anomalies(added: &[AddedLine]) -> Vec<Anomaly> {
    // Group by (op-shape, operator count), first-seen order preserved for stable output. Normally the
    // op-shape alone determines the operator count (one `∘` per flip-op), but a source line can carry a
    // *literal* `∘` (U+2218 — a composition operator, or one in a comment): it renders verbatim into the
    // shape yet records no operator, so two same-shape lines can disagree on `ops.len()`. Keying on the
    // count too keeps every group's operator vectors equal-length (no out-of-bounds) and positionally
    // comparable, and only isolates those pathological lines — for all real code the key is redundant.
    // A line paired with its recovered operator vector (the sites that share one op-shape+count key).
    type OpSites<'a> = Vec<(&'a AddedLine, Vec<String>)>;
    let mut order: Vec<(String, usize)> = Vec::new();
    let mut by_shape: BTreeMap<(String, usize), OpSites> = BTreeMap::new();
    for line in added {
        let (shape, ops) = mask_and_operators(&line.text);
        if ops.is_empty() {
            continue; // no flip-operators on this line ⇒ it can't carry an operator anomaly
        }
        let key = (shape, ops.len());
        if !by_shape.contains_key(&key) {
            order.push(key.clone());
        }
        by_shape.entry(key).or_default().push((line, ops));
    }

    let mut out = Vec::new();
    for key in &order {
        let sites = &by_shape[key];
        let n = sites.len();
        if n < MECHANICAL_MIN {
            continue; // a one-off flip is a substantive line, not a bulk-edit outlier
        }
        // Group key pins the operator count ⇒ equal-length, positionally-aligned operator vectors.
        let width = key.1;
        let mut reasons: Vec<Vec<String>> = vec![Vec::new(); n];
        for p in 0..width {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for (_, ops) in sites {
                *counts.entry(ops[p].as_str()).or_default() += 1;
            }
            let Some((mode, mode_count)) =
                counts.iter().max_by_key(|(_, &c)| c).map(|(m, c)| (*m, *c))
            else {
                continue;
            };
            // Only an overwhelming supermajority makes a minority operator "anomalous"; genuinely
            // varied operators (no dominant value) or a near-even split are expected, not smuggled.
            if mode_count * ANOMALY_SUPERMAJORITY_DEN < n * ANOMALY_SUPERMAJORITY_NUM {
                continue;
            }
            for (idx, (_, ops)) in sites.iter().enumerate() {
                if ops[p] != mode {
                    reasons[idx].push(format!("operator {} where {mode_count}/{n} use {mode}", ops[p]));
                }
            }
        }
        for (idx, rs) in reasons.into_iter().enumerate() {
            if !rs.is_empty() {
                let site = sites[idx].0;
                out.push(Anomaly {
                    file: site.file.clone(),
                    symbol: site.symbol.clone(),
                    text: site.text.clone(),
                    reason: rs.join("; "),
                });
            }
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
    fn anomaly_carries_the_enclosing_symbol() {
        // the smuggled site is inside `fn compute_tax`; the anomaly must carry that symbol
        let mut added: Vec<AddedLine> = (0..8)
            .map(|i| AddedLine { file: "t.rs".into(), text: format!("r{i} = scale(i{i}, 2)"), symbol: Some("fn render".into()) })
            .collect();
        added.push(AddedLine { file: "t.rs".into(), text: "rX = scale(iX, 3)".into(), symbol: Some("fn compute_tax".into()) });
        let a = &summarize(&added).groups[0].anomalies[0];
        assert_eq!(a.symbol.as_deref(), Some("fn compute_tax"));
        assert_eq!(a.text, "rX = scale(iX, 3)");
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

    #[test]
    fn operator_mask_records_flip_operators_and_keeps_structural_ones() {
        // flip operators (comparison + logical) → `∘`, recorded in order; ids/numbers masked as usual
        assert_eq!(mask_and_operators("if (i <= n) {"), ("_ (_ ∘ _) {".to_string(), vec!["<=".to_string()]));
        assert_eq!(
            mask_and_operators("a == b && c != d"),
            ("_ ∘ _ ∘ _ ∘ _".to_string(), vec!["==".to_string(), "&&".to_string(), "!=".to_string()])
        );
        // structural digraphs stay verbatim and record NO operator (so they never regroup/flag)
        assert_eq!(mask_and_operators("a => b").1, Vec::<String>::new());
        assert_eq!(mask_and_operators("f() -> T").1, Vec::<String>::new());
        assert_eq!(mask_and_operators("ch <- v").1, Vec::<String>::new());
        assert_eq!(mask_and_operators("x = y").1, Vec::<String>::new());
        // arithmetic and bitwise are deliberately structural (excluded), so they record NO operator —
        // they vary legitimately line-to-line and would be noise
        assert_eq!(mask_and_operators("w = base + delta").1, Vec::<String>::new());
        assert_eq!(mask_and_operators("m = a & b | c").1, Vec::<String>::new());
        // `==` is a flip (comparison), a bare `=` is not — so they mask to DIFFERENT shapes and an
        // assignment can never regroup with a comparison
        assert_ne!(mask_and_operators("a == b").0, mask_and_operators("a = b").0);
    }

    #[test]
    fn level0_mask_is_unchanged_by_the_tokenizer_refactor() {
        // the operator mask is opt-in; mask_shape (ops verbatim) must be byte-identical to before
        assert_eq!(mask_shape("x >= 0 && y != 1.5"), "_ >= # && _ != #");
        assert_eq!(mask_shape("a => b -> c"), "_ => _ -> _");
        assert_eq!(mask_shape("p += q <<= r"), "_ += _ <<= _");
    }

    #[test]
    fn operator_anomaly_surfaces_a_flipped_comparison() {
        // 9 bound checks use `<=`, one smuggles a `<` — a different Level-0 shape, so it hides as a
        // lone substantive line; the operator pass regroups them and surfaces it.
        let mut added: Vec<String> = (0..9).map(|i| format!("if (i{i} <= n) break;")).collect();
        added.push("if (iX < n) break;".to_string());
        let s = summarize(&al(added));
        assert_eq!(s.operator_anomalies.len(), 1, "exactly the flipped site");
        assert_eq!(s.operator_anomalies[0].text, "if (iX < n) break;");
        assert!(
            s.operator_anomalies[0].reason.contains("operator < where 9/10 use <="),
            "{}",
            s.operator_anomalies[0].reason
        );
        assert_eq!(s.anomaly_count, 1, "operator anomaly counts toward the total");
        // and it's reachable through the unified iterator the CLI uses
        assert!(s.all_anomalies().any(|a| a.text == "if (iX < n) break;"));
    }

    #[test]
    fn logical_operator_flip_is_surfaced() {
        // a guard codemod: most sites AND two conditions, one botched to OR
        let mut added: Vec<String> = (0..7).map(|i| format!("ok{i} = a{i} && b{i}")).collect();
        added.push("okX = aX || bX".to_string());
        let s = summarize(&al(added));
        assert_eq!(s.operator_anomalies.len(), 1);
        assert_eq!(s.operator_anomalies[0].text, "okX = aX || bX");
        assert!(s.operator_anomalies[0].reason.contains("operator || where 7/8 use &&"));
    }

    #[test]
    fn genuinely_varied_operators_are_not_flagged() {
        // a comparison ladder where each site legitimately uses a different operator — no dominant
        // value at the position, so nothing fires (the key false-positive guard)
        let added = vec![
            "r0 = a0 < b0".to_string(),
            "r1 = a1 > b1".to_string(),
            "r2 = a2 <= b2".to_string(),
            "r3 = a3 >= b3".to_string(),
            "r4 = a4 == b4".to_string(),
            "r5 = a5 != b5".to_string(),
        ];
        let s = summarize(&al(added));
        assert_eq!(s.operator_anomalies.len(), 0, "no 80% majority ⇒ no anomaly");
        assert_eq!(s.anomaly_count, 0);
    }

    #[test]
    fn near_even_operator_split_is_not_flagged() {
        // 3-vs-2 at N=5 (60%) is ordinary variation, not a smuggled flip
        let mut added: Vec<String> = (0..3).map(|i| format!("g{i} = x{i} < y{i}")).collect();
        added.extend((0..2).map(|i| format!("h{i} = p{i} > q{i}")));
        let s = summarize(&al(added));
        assert_eq!(s.operator_anomalies.len(), 0, "60% is not an overwhelming majority");
    }

    #[test]
    fn arithmetic_and_bitwise_differences_are_not_flagged() {
        // the realistic false positive: a block of offset computations where one line legitimately
        // subtracts. Arithmetic is excluded from the flip set precisely so this is silent, not noise.
        let mut added: Vec<String> = (0..4).map(|i| format!("r{i} = base + delta{i}")).collect();
        added.push("rX = base - deltaX".to_string()); // legitimate subtraction, 4/5 use `+`
        let s = summarize(&al(added));
        assert_eq!(s.operator_anomalies.len(), 0, "arithmetic variation is not flagged");
        // same for a lone bitwise `|` among `&`s
        let mut bits: Vec<String> = (0..5).map(|i| format!("m{i} = a{i} & MASK")).collect();
        bits.push("mX = aX | MASK".to_string());
        assert_eq!(summarize(&al(bits)).operator_anomalies.len(), 0, "bitwise variation is not flagged");
    }

    #[test]
    fn literal_composition_operator_does_not_panic() {
        // A source line carrying a *literal* `∘` (U+2218) renders that char verbatim into the op-shape
        // but records no operator, so two same-shape lines can disagree on operator count. The grouping
        // key pins the count, so this must NOT panic (regression: it indexed past the operator vector).
        let added = vec![
            "r1 = a1 < b1 < c1".to_string(),
            "r2 = a2 < b2 < c2".to_string(),
            "r3 = a3 < b3 ∘ c3".to_string(), // one flip-op + a literal ∘ ⇒ shorter operator vector
        ];
        let s = summarize(&al(added)); // no panic
        // the two full `< … <` lines are only a 2-site group (< MECHANICAL_MIN), so nothing fires
        assert_eq!(s.operator_anomalies.len(), 0);
    }

    #[test]
    fn assignments_do_not_masquerade_as_operator_anomalies() {
        // a block of plain assignments (bare `=`, structural) carries no flip-operator, so the pass
        // ignores it entirely — an `=` is never mistaken for a flipped `==`
        let added: Vec<String> = (0..6).map(|i| format!("field{i} = init{i}")).collect();
        let s = summarize(&al(added));
        assert_eq!(s.operator_anomalies.len(), 0);
    }

    #[test]
    fn operator_anomaly_carries_file_and_symbol() {
        // the flipped site lives in payments/tax.rs inside `fn compute` — the anomaly must say so
        let mut added: Vec<AddedLine> = (0..8)
            .map(|i| AddedLine {
                file: "ui/row.rs".into(),
                text: format!("keep{i} = lo{i} <= hi{i}"),
                symbol: Some("fn render".into()),
            })
            .collect();
        added.push(AddedLine {
            file: "payments/tax.rs".into(),
            text: "keepX = loX < hiX".into(),
            symbol: Some("fn compute".into()),
        });
        let s = summarize(&added);
        assert_eq!(s.operator_anomalies.len(), 1);
        assert_eq!(s.operator_anomalies[0].file, "payments/tax.rs");
        assert_eq!(s.operator_anomalies[0].symbol.as_deref(), Some("fn compute"));
    }
}
