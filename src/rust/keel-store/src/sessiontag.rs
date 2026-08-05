//! Session retrieval tags (Linear NEW-1076, Epic 4 — the flywheel).
//!
//! A prior session is only useful to a future task if we can *find* it. The validated retrieval is
//! deterministic graph overlap: `score = 2·|shared symbols| + 2·|shared operation-patterns|`, take
//! top-k. That was proven with hand-modeled tags (0→72%); this module is the real extractor the
//! productionization rests on — given a committed change, derive the `sym` set (the identifiers it
//! touched) it should be retrievable by. The bar is **recall@k**, not precision: coarse over-
//! extraction is fine because a strong answerer disambiguates the small top-k.

use crate::object::ObjectId;
use crate::repo::{ChangeKind, Repo};
use crate::store::{Result, StoreError};
use crate::textdiff::{self, Tag};
use std::collections::BTreeSet;

/// The salient identifiers a change touched — the `sym` half of its retrieval tags. Only the
/// **changed** lines are scanned (added/removed on a modify; the whole file on add/delete), so the
/// set reflects what the session actually worked on, not the ambient file. Tokens are lowercased and
/// filtered to salient identifiers (drops language keywords, common noise, and short tokens).
///
/// Known recall gaps (empty set), acceptable for a coarse, recall-oriented extractor: a mode-only
/// change (`chmod +x` — same bytes, no changed lines), a binary-only change (skipped), and a change
/// whose only changed tokens are stopwords/short. Rename (delete+add) and pure deletion DO produce
/// tags (both sides / the parent content are mined).
pub fn changed_symbols(repo: &Repo, change: ObjectId) -> Result<BTreeSet<String>> {
    let c = repo.change(change)?.ok_or(StoreError::Corrupt(change))?;
    let parent = c.parents.first().copied();

    let mut syms = BTreeSet::new();
    for pc in repo.change_files(change)? {
        let new = if pc.kind == ChangeKind::Deleted {
            None
        } else {
            repo.file_bytes_at(change, &pc.path)?
        };
        let old = match (pc.kind, parent) {
            (ChangeKind::Added, _) | (_, None) => None,
            (_, Some(p)) => repo.file_bytes_at(p, &pc.path)?,
        };
        for tok in identifiers(&changed_text(old.as_deref(), new.as_deref())) {
            // Split camelCase / snake_case into component words so `chargeGateway` and `refundGateway`
            // both surface `gateway` — the shared domain word retrieval overlaps on. Whole-identifier
            // matching would miss it (the validated tags are split domain words, not raw identifiers).
            for word in split_identifier(tok) {
                if is_salient(&word) {
                    syms.insert(word);
                }
            }
        }
    }
    Ok(syms)
}

/// Split an identifier into lowercased component words on `_` and camelCase/PascalCase boundaries:
/// `chargeGateway` → [`charge`, `gateway`], `payment_charge` → [`payment`, `charge`], `payment` →
/// [`payment`]. (A run of capitals like `HTTP` stays whole — good enough for recall.)
fn split_identifier(tok: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in tok.split('_') {
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for (i, &ch) in chars.iter().enumerate() {
            let boundary = i > 0
                && ch.is_ascii_uppercase()
                && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            if boundary && !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(ch.to_ascii_lowercase());
        }
        if !cur.is_empty() {
            words.push(cur);
        }
    }
    words
}

/// The text whose identifiers represent the change to `path`: the new content for an add, the old
/// content for a delete, and just the added/removed lines for a modify. Binary blobs (a NUL byte) are
/// skipped so we never mine garbage identifiers out of them.
fn changed_text(old: Option<&[u8]>, new: Option<&[u8]>) -> String {
    let is_text = |b: &[u8]| !b.contains(&0);
    match (old, new) {
        (None, Some(n)) if is_text(n) => String::from_utf8_lossy(n).into_owned(), // added
        (Some(o), None) if is_text(o) => String::from_utf8_lossy(o).into_owned(), // deleted
        (Some(o), Some(n)) if is_text(o) && is_text(n) => {
            // modified: keep only the lines that actually changed (either side).
            let (os, ns) = (String::from_utf8_lossy(o), String::from_utf8_lossy(n));
            let mut out = String::new();
            for hunk in textdiff::diff_lines(&os, &ns) {
                for line in hunk.lines {
                    if matches!(line.tag, Tag::Add | Tag::Del) {
                        out.push_str(&line.text);
                        out.push('\n');
                    }
                }
            }
            out
        }
        _ => String::new(), // binary or empty
    }
}

/// Identifier tokens (`[A-Za-z_][A-Za-z0-9_]*`) in `text`, in order, without allocation.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() && !is_ident_start(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let start = i;
        while i < bytes.len() && is_ident_cont(bytes[i]) {
            i += 1;
        }
        Some(&text[start..i])
    })
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}
fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Keep only words likely to carry domain meaning: length ≥ 3, not a language keyword or a pervasive
/// programming-noise word. `word` is already lowercased by [`split_identifier`]. Deliberately a
/// *small* stoplist — over-filtering costs recall, and the retrieval tolerates noise (top-k + a
/// strong answerer), so we err toward keeping words.
fn is_salient(word: &str) -> bool {
    word.len() >= 3 && !STOPWORDS.contains(&word)
}

/// Language keywords across TS/JS/Py/Go/Rust/C plus a few pervasive noise words. Intentionally short —
/// over-filtering costs recall, and the retrieval tolerates noise (top-k + a strong answerer).
const STOPWORDS: &[&str] = &[
    // control flow / declarations
    "the", "and", "for", "not", "let", "var", "const", "func", "function", "def", "fun", "return",
    "class",
    "struct", "enum", "impl", "trait", "interface", "type", "import", "export", "from", "package",
    "public", "private", "protected", "static", "async", "await", "yield", "new", "this", "self",
    "super", "null", "nil", "none", "true", "false", "void", "int", "str", "bool", "string",
    "number", "float", "double", "char", "byte", "else", "elif", "while", "match", "case", "switch",
    "break", "continue", "default", "try", "catch", "except", "finally", "throw", "raise", "with",
    "use", "using", "namespace", "extends", "implements", "override", "abstract", "final",
    // pervasive programming nouns/verbs that carry little domain signal
    "get", "set", "add", "put", "map", "list", "vec", "arr", "obj", "val", "value", "data", "item",
    "key", "idx", "len", "tmp", "res", "req", "err", "out", "buf", "ctx", "cfg", "args", "opts",
    "name", "size", "kind", "text", "line", "path", "file", "call",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repo;
    use crate::testutil::TmpDir;
    use std::fs;

    #[test]
    fn changed_symbols_extracts_split_domain_words_from_changed_lines() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();

        // c1 (root add): a gateway-charge function.
        fs::write(
            work.path().join("payment.ts"),
            "export function chargeGateway(order) {\n  return payment.charge(order);\n}\n",
        )
        .unwrap();
        let c1 = repo.commit_dir(work.path(), "add charge", "acct", 1, None).unwrap();

        // c2 (modify): append a refund function; only the new lines should be mined.
        fs::write(
            work.path().join("payment.ts"),
            "export function chargeGateway(order) {\n  return payment.charge(order);\n}\n\
             export function refundGateway(order) {\n  return payment.refund(order);\n}\n",
        )
        .unwrap();
        let c2 = repo.commit_dir(work.path(), "add refund", "acct", 2, None).unwrap();

        let s1 = changed_symbols(&repo, c1).unwrap();
        // camelCase `chargeGateway` split into `charge` + `gateway`; domain nouns kept.
        for w in ["charge", "gateway", "payment", "order"] {
            assert!(s1.contains(w), "c1 syms missing {w}: {s1:?}");
        }
        // keywords are filtered out.
        assert!(!s1.contains("export") && !s1.contains("function") && !s1.contains("return"));

        let s2 = changed_symbols(&repo, c2).unwrap();
        // the modify only touched the refund lines → refund present…
        for w in ["refund", "gateway", "payment", "order"] {
            assert!(s2.contains(w), "c2 syms missing {w}: {s2:?}");
        }
        // …and `charge` (an unchanged line) is NOT re-mined — extraction is changed-lines-only.
        assert!(!s2.contains("charge"), "c2 wrongly mined an unchanged line: {s2:?}");
    }
}
