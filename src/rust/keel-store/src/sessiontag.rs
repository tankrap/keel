//! Session retrieval tags (Linear NEW-1076, Epic 4 — the flywheel).
//!
//! A prior session is only useful to a future task if we can *find* it. The validated retrieval is
//! deterministic graph overlap: `score = 2·|shared symbols| + 2·|shared operation-patterns|`, take
//! top-k. That was proven with hand-modeled tags (0→72%); this module is the real extractor the
//! productionization rests on — given a committed change, derive both its `sym` set (the identifiers
//! it touched, [`changed_symbols`]) and its `pat` set (the operation class it performed,
//! [`operation_patterns`]). The bar is **recall@k**, not precision: coarse over-extraction is fine
//! because a strong answerer disambiguates the small top-k. The `examples/flywheel_recall` harness
//! shows real sym + real pat holds recall@2=@3=100% on the validated pool.

use crate::object::ObjectId;
use crate::repo::{ChangeKind, Repo};
use crate::store::{Result, Store, StoreError};
use crate::textdiff::{self, Tag};
use std::collections::{BTreeSet, HashSet};

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
    Ok(symbols_from_text(&change_text(repo, change)?))
}

/// The concatenated changed-line text across every file a change touched (added/removed lines on a
/// modify; whole file on add/delete) — the raw material both the symbol and pattern extractors mine.
fn change_text(repo: &Repo, change: ObjectId) -> Result<String> {
    let c = repo.change(change)?.ok_or(StoreError::Corrupt(change))?;
    let parent = c.parents.first().copied();
    let mut all = String::new();
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
        all.push_str(&changed_text(old.as_deref(), new.as_deref()));
        all.push('\n');
    }
    Ok(all)
}

/// The salient split domain words in arbitrary `text` — the query-side extractor (a new task's target
/// code / task description), and the shared core of [`changed_symbols`]. Same tokenize → camelCase/
/// snake split → salience filter, so a session (indexed from its diff) and a task (from its code)
/// produce comparable tags to overlap.
pub fn symbols_from_text(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_symbols(text, &mut out);
    out
}

/// Tokenize `text`, split each identifier into component words, keep the salient ones. Splitting
/// `chargeGateway`/`refundGateway` both to `gateway` is what lets retrieval overlap on the shared
/// domain word — whole-identifier matching would miss it.
fn collect_symbols(text: &str, out: &mut BTreeSet<String>) {
    for tok in identifiers(text) {
        for word in split_identifier(tok) {
            if is_salient(&word) {
                out.insert(word);
            }
        }
    }
}

/// The operation-pattern class(es) a change performed — the `pat` half of its retrieval tags. This
/// is the coarse *kind* of operation, which lets a task retrieve a prior session whose symbols are
/// disjoint (e.g. a new CREATE-TABLE retrieves a UUID-policy session — both `schema-create`). Drawn
/// from a small controlled vocabulary; classified by signal keywords in the change's text. See
/// [`patterns_from_text`].
pub fn operation_patterns(repo: &Repo, change: ObjectId) -> Result<BTreeSet<String>> {
    Ok(patterns_from_text(&change_text(repo, change)?))
}

/// The operation-pattern class(es) evident in arbitrary `text` — the query-side classifier (a new
/// task's target code / description) and the shared core of [`operation_patterns`]. A change can
/// match several classes; retrieval scores on any overlap. Deliberately signal-keyword based (cheap,
/// deterministic, auditable) — a first cut the recall@k experiment measures before escalating to an
/// AST/model classifier.
pub fn patterns_from_text(text: &str) -> BTreeSet<String> {
    let lower = text.to_ascii_lowercase();
    let has = |sigs: &[&str]| sigs.iter().any(|s| lower.contains(s));
    let mut pats = BTreeSet::new();
    // The blocks are independent (a change can carry several classes); order is irrelevant. Signals
    // err toward recall — `http`/`schema` will over-fire on urls / zod-style schemas, which is
    // acceptable (a spurious class only adds a weak overlap the answerer ignores), but signals that
    // fire on *pervasive* code (`.find(` = Array.find; bare `tax` matches `syntax`) are excluded.
    // schema-create — DDL / migrations.
    if has(&[
        "create table", "createtable", "migrat", "alter table", "altercolumn", "add column",
        "schema", "create index", "createindex", "uuidv7", "auto-increment", "autoincrement",
    ]) {
        pats.insert("schema-create".into());
    }
    // db-query — reads / filters / pagination. (`.find(` omitted: Array.prototype.find is everywhere.)
    if has(&[".query(", ".where(", "select ", "findmany", "findunique", "queryraw", "keyset", "pagination", "order by", "orderby"]) {
        pats.insert("db-query".into());
    }
    // external-call — network / external service / message-bus calls.
    if has(&["fetch(", "axios", "http", ".send(", ".post(", "gateway.", "provider.", "enqueue", "analytics", "emailservice", "webhook", "requests.", "urllib", "i18n.render", "smsprovider"]) {
        pats.insert("external-call".into());
    }
    // money — monetary values. Kept tight: `amount`/`total` are too generic, and bare `tax` would
    // match `syntax`, so use unambiguous money words + specific tax compounds.
    if has(&["cents", "price", "money", "invoice", "currency", "monetary", "taxrate", "tax_rate", "salestax", "regionaltax"]) {
        pats.insert("money".into());
    }
    pats
}

// ── persistence: the change→tags side-table (address-stable, like lesson_help / informed) ────────

/// aux-KV namespaces. `symtag`/`pattag` are the **forward** index (change id → its tag set, the
/// source of truth for scoring); `symidx`/`patidx` are the **inverted** index (a tag → the change ids
/// carrying it) so `retrieve` visits only changes that share ≥1 query tag instead of scanning all of
/// history. Side-tables so indexing never moves a change's content address — like `lesson_help`.
const NS_SYM: &str = "symtag";
const NS_PAT: &str = "pattag";
const NS_SYMIDX: &str = "symidx";
const NS_PATIDX: &str = "patidx";

/// Extract a change's `sym` + `pat` tags, persist them keyed by the change id, and reconcile the
/// inverted index. Idempotent (re-indexing recomputes and reconciles postings). The posting updates
/// and the forward writes are separate txns (no cross-put atomicity); a crash mid-index can leave
/// them transiently inconsistent, which only costs a harmless extra candidate or a recoverable miss —
/// [`reindex_all`] is the repair path. The forward index (`symtag`/`pattag`) is the source of truth.
pub fn index_change(repo: &Repo, change: ObjectId) -> Result<()> {
    let new_syms = changed_symbols(repo, change)?;
    let new_pats = operation_patterns(repo, change)?;
    let store = repo.store();
    // Reconcile postings against the change's previously-indexed tags (empty on first index).
    let (old_syms, old_pats) = change_tags(store, change)?;
    update_postings(store, NS_SYMIDX, change, &old_syms, &new_syms)?;
    update_postings(store, NS_PATIDX, change, &old_pats, &new_pats)?;
    // Forward index (the source of truth used for scoring).
    store.aux_put(NS_SYM, &change.0, join_tags(&new_syms).as_bytes())?;
    store.aux_put(NS_PAT, &change.0, join_tags(&new_pats).as_bytes())?;
    Ok(())
}

/// Ensure `change` is in the posting list of every tag it now has (idempotent), and remove it from
/// any tag it no longer has — reconciling the inverted index with the change's new tag set. Inserting
/// for *all* current tags (not just the delta) also rebuilds a missing inverted index on re-index.
fn update_postings(
    store: &Store,
    ns: &str,
    change: ObjectId,
    old: &BTreeSet<String>,
    new: &BTreeSet<String>,
) -> Result<()> {
    for tag in new {
        posting_insert(store, ns, tag, change)?;
    }
    for tag in old.difference(new) {
        posting_remove(store, ns, tag, change)?;
    }
    Ok(())
}

fn posting_insert(store: &Store, ns: &str, tag: &str, change: ObjectId) -> Result<()> {
    let mut ids = read_postings(store, ns, tag)?;
    if !ids.contains(&change) {
        ids.push(change);
        store.aux_put(ns, tag.as_bytes(), &encode_ids(&ids))?;
    }
    Ok(())
}

fn posting_remove(store: &Store, ns: &str, tag: &str, change: ObjectId) -> Result<()> {
    let mut ids = read_postings(store, ns, tag)?;
    if let Some(pos) = ids.iter().position(|x| *x == change) {
        ids.swap_remove(pos);
        if ids.is_empty() {
            store.aux_delete(ns, tag.as_bytes())?;
        } else {
            store.aux_put(ns, tag.as_bytes(), &encode_ids(&ids))?;
        }
    }
    Ok(())
}

fn read_postings(store: &Store, ns: &str, tag: &str) -> Result<Vec<ObjectId>> {
    Ok(store.aux_get(ns, tag.as_bytes())?.map(|b| decode_ids(&b)).unwrap_or_default())
}

/// Posting lists are raw concatenated 32-byte change ids.
fn encode_ids(ids: &[ObjectId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 32);
    for id in ids {
        out.extend_from_slice(&id.0);
    }
    out
}
fn decode_ids(bytes: &[u8]) -> Vec<ObjectId> {
    bytes
        .chunks_exact(32)
        .map(|c| {
            let mut a = [0u8; 32];
            a.copy_from_slice(c);
            ObjectId(a)
        })
        .collect()
}

/// A change's persisted `(sym, pat)` tags, or empty sets if it was never indexed.
pub fn change_tags(store: &Store, change: ObjectId) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let syms = store.aux_get(NS_SYM, &change.0)?.map(|b| split_tags(&b)).unwrap_or_default();
    let pats = store.aux_get(NS_PAT, &change.0)?.map(|b| split_tags(&b)).unwrap_or_default();
    Ok((syms, pats))
}

/// (Re)index every change reachable from head — the backfill for an existing repo and a repair path.
/// Walks the full DAG (`ancestors`, every parent edge), not just first-parent, so a merged-in side
/// branch's changes are indexed too. Returns how many changes were indexed.
pub fn reindex_all(repo: &Repo) -> Result<usize> {
    let ids = match repo.head()? {
        Some(h) => repo.ancestors(h)?,
        None => Vec::new(),
    };
    for id in &ids {
        index_change(repo, *id)?;
    }
    Ok(ids.len())
}

/// Retrieve the indexed changes whose tags best overlap the query — `score = 2·|shared sym| +
/// 2·|shared pat|` — highest first, limited to `k` and score > 0. This is the deterministic graph
/// retrieval NEW-1076 validated (recall@k over a small candidate set; a strong reader disambiguates).
/// It surfaces a session whose *symbols/patterns* match even when its files aren't import-neighbors
/// of the target — the cross-domain retrieval the file-neighborhood pass structurally can't do.
pub fn retrieve(
    store: &Store,
    query_sym: &BTreeSet<String>,
    query_pat: &BTreeSet<String>,
    k: usize,
) -> Result<Vec<(ObjectId, i32)>> {
    // Candidate changes: the union of the query tags' posting lists — only changes that share ≥1
    // tag, instead of every indexed change. Then score each via its forward tags.
    let mut candidates: HashSet<ObjectId> = HashSet::new();
    for w in query_sym {
        candidates.extend(read_postings(store, NS_SYMIDX, w)?);
    }
    for p in query_pat {
        candidates.extend(read_postings(store, NS_PATIDX, p)?);
    }
    let mut scored: Vec<(ObjectId, i32)> = Vec::new();
    for cid in candidates {
        let (syms, pats) = change_tags(store, cid)?;
        let s = syms.intersection(query_sym).count() as i32;
        let p = pats.intersection(query_pat).count() as i32;
        let score = 2 * s + 2 * p;
        if score > 0 {
            scored.push((cid, score));
        }
    }
    // highest score first; deterministic id tiebreak (matches the validated lab's ordering)
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0 .0.cmp(&b.0 .0)));
    scored.truncate(k);
    Ok(scored)
}

/// Tags are newline-joined — a tag (a lowercased identifier word, or a `pat` class like
/// `external-call`) never contains a newline — and sorted by `BTreeSet` iteration, so the encoding
/// is stable.
fn join_tags(tags: &BTreeSet<String>) -> String {
    tags.iter().cloned().collect::<Vec<_>>().join("\n")
}
fn split_tags(bytes: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(bytes).lines().filter(|l| !l.is_empty()).map(String::from).collect()
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
    #[test]
    fn patterns_from_text_classifies_the_operation_kind() {
        let p = patterns_from_text;
        assert!(p("return gateway.charge(order.amount)").contains("external-call"));
        assert!(p("await analyticsQueue.enqueue({ event })").contains("external-call"));
        assert!(p("db.alterColumn('users', 'id', uuidv7())").contains("schema-create"));
        assert!(p("Define the schema for a new orders table").contains("schema-create"));
        assert!(p("return db.query('accounts').where({ tenantId })").contains("db-query"));
        assert!(p("return items.reduce((cents, it) => cents + it.priceCents, 0)").contains("money"));
        // no operation signal → no pattern (the classifier abstains rather than guessing).
        assert!(p("function add(a, b) { return a + b; }").is_empty());
        // `amount`/`total` alone are too generic to imply money; `syntax` must NOT match `tax`.
        assert!(!p("const total = amount + fee").contains("money"));
        assert!(!p("// fix the syntax error in the parser").contains("money"));
        // `.find(` (Array.prototype.find) must NOT read as a db-query.
        assert!(!p("const u = users.find(x => x.active)").contains("db-query"));
    }

    use crate::repo::Repo;
    use crate::testutil::TmpDir;
    use std::fs;

    #[test]
    fn retrieve_narrows_to_overlapping_changes_via_inverted_index() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::write(work.path().join("payment.ts"), "function chargeGateway(order) { return gateway.charge(order); }").unwrap();
        let c1 = repo.commit_dir(work.path(), "gateway", "a", 1, None).unwrap();
        fs::write(work.path().join("mailer.ts"), "function sendWelcome(recipient) { return mailer.deliver(recipient); }").unwrap();
        let c2 = repo.commit_dir(work.path(), "mailer", "a", 2, None).unwrap();
        index_change(&repo, c1).unwrap();
        index_change(&repo, c2).unwrap();
        index_change(&repo, c1).unwrap(); // idempotent re-index must not duplicate postings

        let qsym = symbols_from_text("refund an order via the gateway");
        let empty: BTreeSet<String> = BTreeSet::new();
        let hits = retrieve(repo.store(), &qsym, &empty, 5).unwrap();
        // c1 (shares gateway/order) is retrieved exactly once via the posting lists; c2
        // (mailer/recipient) shares no query tag, so the inverted index never even visits it.
        assert_eq!(hits.iter().filter(|(id, _)| *id == c1).count(), 1, "c1 retrieved once: {hits:?}");
        assert!(!hits.iter().any(|(id, _)| *id == c2), "c2 excluded (no shared tag): {hits:?}");
    }

    #[test]
    fn index_change_persists_and_reads_back_tags() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::write(
            work.path().join("payment.ts"),
            "export function chargeGateway(order) { return gateway.charge(order.amount); }",
        )
        .unwrap();
        let c = repo.commit_dir(work.path(), "charge", "acct", 1, None).unwrap();

        // not indexed yet → empty tags
        let (s0, p0) = change_tags(repo.store(), c).unwrap();
        assert!(s0.is_empty() && p0.is_empty());

        index_change(&repo, c).unwrap();
        let (syms, pats) = change_tags(repo.store(), c).unwrap();
        assert!(syms.contains("gateway") && syms.contains("charge"), "syms: {syms:?}");
        assert!(pats.contains("external-call"), "pats: {pats:?}");

        // reindex_all re-derives them (idempotent) and reports the count
        assert_eq!(reindex_all(&repo).unwrap(), 1);
        assert_eq!(change_tags(repo.store(), c).unwrap().0, syms);
    }

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
