//! keel object model + canonical binary encoding.
//!
//! Wire form: `encode(obj) = [KIND] ++ body`. `id(obj) = BLAKE3(encode(obj))`.
//! The KIND byte is hashed, so a blob and a tree with otherwise-identical bytes
//! never collide. Structured objects (tree/change/session/review) use length-prefixed,
//! canonically-ordered fields so the encoding is byte-stable regardless of how the
//! in-memory value was built. Blobs are `[KIND_BLOB] ++ raw content`.

use std::fmt;

pub type Result<T> = std::result::Result<T, DecodeError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// ran out of bytes mid-field
    Truncated,
    /// unknown object kind tag
    BadKind(u8),
    /// a varint that would overflow u64
    VarintOverflow,
    /// a bad option/enum discriminant
    BadTag(u8),
    /// invalid UTF-8 in a string field
    BadUtf8,
    /// bytes left over after decoding a fixed-shape object
    TrailingBytes,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "truncated object"),
            DecodeError::BadKind(k) => write!(f, "unknown object kind {k}"),
            DecodeError::VarintOverflow => write!(f, "varint overflow"),
            DecodeError::BadTag(t) => write!(f, "bad discriminant {t}"),
            DecodeError::BadUtf8 => write!(f, "invalid utf-8"),
            DecodeError::TrailingBytes => write!(f, "trailing bytes after object"),
        }
    }
}
impl std::error::Error for DecodeError {}

pub const KIND_BLOB: u8 = 1;
pub const KIND_TREE: u8 = 2;
pub const KIND_CHANGE: u8 = 3;
pub const KIND_SESSION: u8 = 4;
pub const KIND_REVIEW: u8 = 5;

// ── ObjectId ─────────────────────────────────────────────────────────────────

/// A 32-byte BLAKE3 content address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for &b in &self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0xf) as usize] as char);
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<ObjectId> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        let bytes = s.as_bytes();
        for i in 0..32 {
            let hi = hex_val(bytes[i * 2])?;
            let lo = hex_val(bytes[i * 2 + 1])?;
            out[i] = (hi << 4) | lo;
        }
        Some(ObjectId(out))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}
impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

// ── field types ──────────────────────────────────────────────────────────────

/// Verification state carried by a change or session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verification {
    Unverified,
    Green,
    Red,
}
impl Verification {
    /// Wire tag (also the side-table encoding).
    pub fn tag(self) -> u8 {
        match self {
            Verification::Unverified => 0,
            Verification::Green => 1,
            Verification::Red => 2,
        }
    }
    fn from_tag(t: u8) -> Result<Verification> {
        Self::from_u8(t).ok_or(DecodeError::BadTag(t))
    }
    /// Decode a tag byte, or `None` if it isn't a valid verification.
    pub fn from_u8(t: u8) -> Option<Verification> {
        match t {
            0 => Some(Verification::Unverified),
            1 => Some(Verification::Green),
            2 => Some(Verification::Red),
            _ => None,
        }
    }
}

/// A reviewer's verdict on a session or change.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Reject,
    Comment,
}
impl Verdict {
    /// Wire tag.
    pub fn tag(self) -> u8 {
        match self {
            Verdict::Approve => 0,
            Verdict::RequestChanges => 1,
            Verdict::Reject => 2,
            Verdict::Comment => 3,
        }
    }
    fn from_tag(t: u8) -> Result<Verdict> {
        Self::from_u8(t).ok_or(DecodeError::BadTag(t))
    }
    /// Decode a tag byte, or `None` if it isn't a valid verdict.
    pub fn from_u8(t: u8) -> Option<Verdict> {
        match t {
            0 => Some(Verdict::Approve),
            1 => Some(Verdict::RequestChanges),
            2 => Some(Verdict::Reject),
            3 => Some(Verdict::Comment),
            _ => None,
        }
    }
    /// Lowercase name, for CLI/JSON.
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request-changes",
            Verdict::Reject => "reject",
            Verdict::Comment => "comment",
        }
    }
    /// Parse from the CLI/JSON name.
    pub fn from_name(s: &str) -> Option<Verdict> {
        match s {
            "approve" => Some(Verdict::Approve),
            "request-changes" => Some(Verdict::RequestChanges),
            "reject" => Some(Verdict::Reject),
            "comment" => Some(Verdict::Comment),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeEntry {
    pub name: String,
    pub mode: u32,
    pub id: ObjectId,
}

/// A directory: entries are canonically sorted by name on encode.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

/// The jj-style change — a snapshot (`tree`) with intent + provenance + verification.
/// Parents are canonically sorted on encode.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Change {
    pub parents: Vec<ObjectId>,
    pub tree: ObjectId,
    pub session: Option<ObjectId>,
    pub intent: String,
    pub author: String,
    pub timestamp: u64,
    pub verification: Verification,
}

/// The agent session artifact (subset of the full NEW-1086 schema; large payloads
/// like the transcript and tool outputs are referenced as blobs, not inlined).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Session {
    pub task: String,
    pub model: String,
    /// the non-obvious constraint / rationale this session discovered — the flywheel fuel
    /// retrieved for related future tasks (may be empty).
    pub lesson: String,
    pub prompts: Option<ObjectId>,
    pub context_served: Option<ObjectId>,
    pub tool_calls: Vec<ObjectId>,
    pub tool_results: Vec<ObjectId>,
    pub verification: Verification,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// A review of a session or change, as a first-class object (peer to [`Session`]). One reviewer,
/// one verdict; several reviews of the same `target` with differing verdicts express disagreement.
/// Large detail (per-finding write-ups) is referenced as blobs, not inlined; the `summary` and
/// `labels` stay inline so the store can answer queries ("reviews mentioning race conditions",
/// "security-critical reviews", "approved without a human") without fetching every finding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Review {
    /// The session or change under review.
    pub target: ObjectId,
    /// The reviewing model or actor (e.g. "claude-opus-4-8", "human:alice").
    pub reviewer: String,
    /// Whether a human performed or signed off this review (vs a fully automated one).
    pub by_human: bool,
    pub verdict: Verdict,
    /// One-line, searchable summary of the review.
    pub summary: String,
    /// Topic labels (e.g. "security", "concurrency"), order preserved.
    pub labels: Vec<String>,
    /// Detailed findings, referenced as blobs.
    pub findings: Vec<ObjectId>,
}

/// A content-addressed object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Object {
    Blob(Vec<u8>),
    Tree(Tree),
    Change(Change),
    Session(Session),
    Review(Review),
}

impl Object {
    pub fn kind(&self) -> u8 {
        match self {
            Object::Blob(_) => KIND_BLOB,
            Object::Tree(_) => KIND_TREE,
            Object::Change(_) => KIND_CHANGE,
            Object::Session(_) => KIND_SESSION,
            Object::Review(_) => KIND_REVIEW,
        }
    }

    /// Canonical, byte-stable wire encoding: `[KIND] ++ body`.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        o.push(self.kind());
        match self {
            Object::Blob(bytes) => o.extend_from_slice(bytes),
            Object::Tree(t) => {
                let mut es = t.entries.clone();
                es.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
                put_uvarint(&mut o, es.len() as u64);
                for e in &es {
                    put_bytes(&mut o, e.name.as_bytes());
                    put_uvarint(&mut o, e.mode as u64);
                    o.extend_from_slice(&e.id.0);
                }
            }
            Object::Change(c) => {
                // parent ORDER is significant (first parent = mainline) — do NOT sort.
                put_uvarint(&mut o, c.parents.len() as u64);
                for p in &c.parents {
                    o.extend_from_slice(&p.0);
                }
                o.extend_from_slice(&c.tree.0);
                put_opt_id(&mut o, &c.session);
                put_bytes(&mut o, c.intent.as_bytes());
                put_bytes(&mut o, c.author.as_bytes());
                put_uvarint(&mut o, c.timestamp);
                o.push(c.verification.tag());
            }
            Object::Session(s) => {
                put_bytes(&mut o, s.task.as_bytes());
                put_bytes(&mut o, s.model.as_bytes());
                put_bytes(&mut o, s.lesson.as_bytes());
                put_opt_id(&mut o, &s.prompts);
                put_opt_id(&mut o, &s.context_served);
                put_vec_id(&mut o, &s.tool_calls);
                put_vec_id(&mut o, &s.tool_results);
                o.push(s.verification.tag());
                put_uvarint(&mut o, s.tokens_in);
                put_uvarint(&mut o, s.tokens_out);
            }
            Object::Review(rv) => {
                o.extend_from_slice(&rv.target.0);
                put_bytes(&mut o, rv.reviewer.as_bytes());
                o.push(rv.by_human as u8);
                o.push(rv.verdict.tag());
                put_bytes(&mut o, rv.summary.as_bytes());
                put_vec_str(&mut o, &rv.labels);
                put_vec_id(&mut o, &rv.findings);
            }
        }
        o
    }

    /// Content address = BLAKE3 of the canonical encoding.
    pub fn id(&self) -> ObjectId {
        ObjectId(*blake3::hash(&self.encode()).as_bytes())
    }

    /// Decode from the canonical wire encoding.
    pub fn decode(bytes: &[u8]) -> Result<Object> {
        let (&kind, rest) = bytes.split_first().ok_or(DecodeError::Truncated)?;
        match kind {
            KIND_BLOB => Ok(Object::Blob(rest.to_vec())),
            KIND_TREE => {
                let mut r = Reader::new(rest);
                let n = r.uvarint()? as usize;
                let mut entries = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    let name = r.string()?;
                    let mode = r.uvarint()? as u32;
                    let id = r.id()?;
                    entries.push(TreeEntry { name, mode, id });
                }
                r.finish(Object::Tree(Tree { entries }))
            }
            KIND_CHANGE => {
                let mut r = Reader::new(rest);
                let np = r.uvarint()? as usize;
                let mut parents = Vec::with_capacity(np.min(1 << 16));
                for _ in 0..np {
                    parents.push(r.id()?);
                }
                let tree = r.id()?;
                let session = r.opt_id()?;
                let intent = r.string()?;
                let author = r.string()?;
                let timestamp = r.uvarint()?;
                let verification = Verification::from_tag(r.u8()?)?;
                r.finish(Object::Change(Change {
                    parents,
                    tree,
                    session,
                    intent,
                    author,
                    timestamp,
                    verification,
                }))
            }
            KIND_SESSION => {
                let mut r = Reader::new(rest);
                let task = r.string()?;
                let model = r.string()?;
                let lesson = r.string()?;
                let prompts = r.opt_id()?;
                let context_served = r.opt_id()?;
                let tool_calls = r.vec_id()?;
                let tool_results = r.vec_id()?;
                let verification = Verification::from_tag(r.u8()?)?;
                let tokens_in = r.uvarint()?;
                let tokens_out = r.uvarint()?;
                r.finish(Object::Session(Session {
                    task,
                    model,
                    lesson,
                    prompts,
                    context_served,
                    tool_calls,
                    tool_results,
                    verification,
                    tokens_in,
                    tokens_out,
                }))
            }
            KIND_REVIEW => {
                let mut r = Reader::new(rest);
                let target = r.id()?;
                let reviewer = r.string()?;
                let by_human = r.bool()?;
                let verdict = Verdict::from_tag(r.u8()?)?;
                let summary = r.string()?;
                let labels = r.vec_str()?;
                let findings = r.vec_id()?;
                r.finish(Object::Review(Review {
                    target,
                    reviewer,
                    by_human,
                    verdict,
                    summary,
                    labels,
                    findings,
                }))
            }
            k => Err(DecodeError::BadKind(k)),
        }
    }
}

// ── encoding helpers ─────────────────────────────────────────────────────────

fn put_uvarint(o: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        o.push(b);
        if v == 0 {
            break;
        }
    }
}

fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_uvarint(o, b.len() as u64);
    o.extend_from_slice(b);
}

fn put_opt_id(o: &mut Vec<u8>, id: &Option<ObjectId>) {
    match id {
        None => o.push(0),
        Some(id) => {
            o.push(1);
            o.extend_from_slice(&id.0);
        }
    }
}

fn put_vec_id(o: &mut Vec<u8>, ids: &[ObjectId]) {
    put_uvarint(o, ids.len() as u64);
    for id in ids {
        o.extend_from_slice(&id.0);
    }
}

fn put_vec_str(o: &mut Vec<u8>, ss: &[String]) {
    put_uvarint(o, ss.len() as u64);
    for s in ss {
        put_bytes(o, s.as_bytes());
    }
}

// ── decoding cursor ──────────────────────────────────────────────────────────

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, i: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.i.checked_add(n).ok_or(DecodeError::Truncated)?;
        if end > self.b.len() {
            return Err(DecodeError::Truncated);
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn uvarint(&mut self) -> Result<u64> {
        let mut x: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 || (shift == 63 && b > 1) {
                return Err(DecodeError::VarintOverflow);
            }
            x |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        Ok(x)
    }

    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.uvarint()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn string(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| DecodeError::BadUtf8)
    }

    fn id(&mut self) -> Result<ObjectId> {
        let s = self.take(32)?;
        let mut a = [0u8; 32];
        a.copy_from_slice(s);
        Ok(ObjectId(a))
    }

    fn opt_id(&mut self) -> Result<Option<ObjectId>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.id()?)),
            t => Err(DecodeError::BadTag(t)),
        }
    }

    fn vec_id(&mut self) -> Result<Vec<ObjectId>> {
        let n = self.uvarint()? as usize;
        let mut v = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            v.push(self.id()?);
        }
        Ok(v)
    }

    fn vec_str(&mut self) -> Result<Vec<String>> {
        let n = self.uvarint()? as usize;
        let mut v = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            v.push(self.string()?);
        }
        Ok(v)
    }

    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            t => Err(DecodeError::BadTag(t)),
        }
    }

    /// Return `obj` only if all bytes were consumed (no trailing garbage).
    fn finish(&self, obj: Object) -> Result<Object> {
        if self.i == self.b.len() {
            Ok(obj)
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId([byte; 32])
    }

    fn sample_tree() -> Object {
        Object::Tree(Tree {
            entries: vec![
                TreeEntry { name: "readme.md".into(), mode: 0o100644, id: oid(0xaa) },
                TreeEntry { name: "src".into(), mode: 0o040000, id: oid(0xbb) },
            ],
        })
    }

    fn sample_change() -> Object {
        Object::Change(Change {
            parents: vec![oid(1), oid(2)],
            tree: oid(3),
            session: Some(oid(4)),
            intent: "fix the flaky refund path".into(),
            author: "acct:justin".into(),
            timestamp: 1_753_800_000,
            verification: Verification::Green,
        })
    }

    fn sample_session() -> Object {
        Object::Session(Session {
            task: "implement refund()".into(),
            model: "claude-opus-4-8".into(),
            lesson: "the gateway needs a 50ms settle delay".into(),
            prompts: Some(oid(9)),
            context_served: None,
            tool_calls: vec![oid(10), oid(11)],
            tool_results: vec![oid(12)],
            verification: Verification::Green,
            tokens_in: 12_345,
            tokens_out: 678,
        })
    }

    fn sample_review() -> Object {
        Object::Review(Review {
            target: oid(9),
            reviewer: "claude-sonnet-5".into(),
            by_human: false,
            verdict: Verdict::RequestChanges,
            summary: "possible race condition on the settle-delay retry".into(),
            labels: vec!["concurrency".into(), "security".into()],
            findings: vec![oid(20), oid(21)],
        })
    }

    #[test]
    fn id_is_deterministic() {
        let b = Object::Blob(b"hello keel".to_vec());
        assert_eq!(b.id(), b.id());
        assert_eq!(b.encode(), b.encode());
        assert_ne!(b.id(), Object::Blob(b"hello keeL".to_vec()).id());
    }

    #[test]
    fn round_trips() {
        for obj in [
            Object::Blob(b"".to_vec()),
            Object::Blob(b"some file content\n".to_vec()),
            sample_tree(),
            sample_change(),
            sample_session(),
            sample_review(),
        ] {
            let decoded = Object::decode(&obj.encode()).expect("decode");
            assert_eq!(decoded, obj, "round-trip failed for {:?}", obj.kind());
        }
    }

    #[test]
    fn review_fields_are_addressed() {
        // Any field a query relies on must change the content address.
        let base = match sample_review() {
            Object::Review(r) => r,
            _ => unreachable!(),
        };
        let id0 = Object::Review(base.clone()).id();

        let mut verdict = base.clone();
        verdict.verdict = Verdict::Approve;
        assert_ne!(Object::Review(verdict).id(), id0, "verdict must be addressed");

        let mut human = base.clone();
        human.by_human = true;
        assert_ne!(Object::Review(human).id(), id0, "by_human must be addressed");

        let mut labels = base.clone();
        labels.labels = vec!["security".into(), "concurrency".into()]; // same set, different order
        assert_ne!(Object::Review(labels).id(), id0, "label order is significant");

        let mut target = base;
        target.target = oid(0xee);
        assert_ne!(Object::Review(target).id(), id0, "target must be addressed");
    }

    #[test]
    fn tree_ordering_is_canonical() {
        // same entries, different insertion order → identical encoding and id
        let a = Object::Tree(Tree {
            entries: vec![
                TreeEntry { name: "a".into(), mode: 0o100644, id: oid(1) },
                TreeEntry { name: "b".into(), mode: 0o100644, id: oid(2) },
            ],
        });
        let b = Object::Tree(Tree {
            entries: vec![
                TreeEntry { name: "b".into(), mode: 0o100644, id: oid(2) },
                TreeEntry { name: "a".into(), mode: 0o100644, id: oid(1) },
            ],
        });
        assert_eq!(a.encode(), b.encode());
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn change_parent_order_is_significant() {
        let base = match sample_change() {
            Object::Change(c) => c,
            _ => unreachable!(),
        };
        let mut a = base.clone();
        a.parents = vec![oid(1), oid(2)];
        let mut b = base.clone();
        b.parents = vec![oid(2), oid(1)];
        // first parent = mainline; swapping parents is a different change
        assert_ne!(Object::Change(a.clone()).id(), Object::Change(b).id());
        // but the same order is deterministic
        let mut a2 = base;
        a2.parents = vec![oid(1), oid(2)];
        assert_eq!(Object::Change(a).id(), Object::Change(a2).id());
    }

    #[test]
    fn kind_prefix_prevents_cross_type_collision() {
        // A blob whose bytes equal a tree body must not share the tree's id.
        let tree = sample_tree();
        let mut body = tree.encode();
        body.remove(0); // strip the KIND byte → raw tree body
        let blob = Object::Blob(body);
        assert_ne!(blob.id(), tree.id());
    }

    #[test]
    fn decode_rejects_trailing_and_truncated() {
        let mut enc = sample_change().encode();
        enc.push(0xff); // trailing garbage
        assert_eq!(Object::decode(&enc), Err(DecodeError::TrailingBytes));

        let enc = sample_session().encode();
        assert_eq!(Object::decode(&enc[..enc.len() - 1]), Err(DecodeError::Truncated));

        assert_eq!(Object::decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(Object::decode(&[99]), Err(DecodeError::BadKind(99)));
    }

    #[test]
    fn hex_round_trips() {
        let id = sample_change().id();
        assert_eq!(ObjectId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(id.to_hex().len(), 64);
        assert_eq!(ObjectId::from_hex("nothex"), None);
    }

    #[test]
    fn golden_blob_id_is_stable() {
        // Pins the exact address of a fixed object. If the canonical encoding ever
        // changes, this fails — that is the point (content addresses must not drift).
        let id = Object::Blob(b"keel".to_vec()).id();
        assert_eq!(
            id.to_hex(),
            "6b229988e49b9188f2ff4d9c4f4a40cc3d2cd03f47709bcef7cd94fae6a22307",
            "blob id changed — canonical encoding drifted"
        );
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
    }

    fn rid(r: &mut Rng) -> ObjectId {
        let mut a = [0u8; 32];
        for x in &mut a {
            *x = (r.next() & 0xff) as u8;
        }
        ObjectId(a)
    }
    fn rstr(r: &mut Rng) -> String {
        let n = (r.next() % 12) as usize;
        (0..n).map(|_| (b'a' + (r.next() % 26) as u8) as char).collect()
    }
    fn roptid(r: &mut Rng) -> Option<ObjectId> {
        if r.next().is_multiple_of(2) {
            Some(rid(r))
        } else {
            None
        }
    }
    fn rvecid(r: &mut Rng) -> Vec<ObjectId> {
        (0..(r.next() % 4)).map(|_| rid(r)).collect()
    }
    fn rvecstr(r: &mut Rng) -> Vec<String> {
        (0..(r.next() % 4)).map(|_| rstr(r)).collect()
    }
    fn rverdict(r: &mut Rng) -> Verdict {
        [Verdict::Approve, Verdict::RequestChanges, Verdict::Reject, Verdict::Comment]
            [(r.next() % 4) as usize]
    }
    fn rverif(r: &mut Rng) -> Verification {
        [Verification::Unverified, Verification::Green, Verification::Red][(r.next() % 3) as usize]
    }
    fn random_object(r: &mut Rng) -> Object {
        match r.next() % 5 {
            0 => {
                let n = (r.next() % 40) as usize;
                Object::Blob((0..n).map(|_| (r.next() & 0xff) as u8).collect())
            }
            1 => Object::Tree(Tree {
                entries: (0..(r.next() % 5))
                    .map(|_| TreeEntry {
                        name: rstr(r),
                        mode: (r.next() % 0o777_777) as u32,
                        id: rid(r),
                    })
                    .collect(),
            }),
            2 => Object::Change(Change {
                parents: (0..(r.next() % 4)).map(|_| rid(r)).collect(),
                tree: rid(r),
                session: roptid(r),
                intent: rstr(r),
                author: rstr(r),
                timestamp: r.next(),
                verification: rverif(r),
            }),
            3 => Object::Session(Session {
                task: rstr(r),
                model: rstr(r),
                lesson: rstr(r),
                prompts: roptid(r),
                context_served: roptid(r),
                tool_calls: rvecid(r),
                tool_results: rvecid(r),
                verification: rverif(r),
                tokens_in: r.next(),
                tokens_out: r.next(),
            }),
            _ => Object::Review(Review {
                target: rid(r),
                reviewer: rstr(r),
                by_human: r.next().is_multiple_of(2),
                verdict: rverdict(r),
                summary: rstr(r),
                labels: rvecstr(r),
                findings: rvecid(r),
            }),
        }
    }

    #[test]
    fn fuzz_decode_never_panics() {
        let mut r = Rng(0xF022_1122_3344_5566);
        // arbitrary random bytes
        for _ in 0..20_000 {
            let len = (r.next() % 300) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| (r.next() & 0xff) as u8).collect();
            let _ = Object::decode(&bytes);
        }
        // single-byte mutations + truncations of valid encodings (targets the parser)
        let valids =
            [sample_tree(), sample_change(), sample_session(), sample_review(), Object::Blob(vec![1, 2, 3, 4])];
        for v in &valids {
            let enc = v.encode();
            for _ in 0..3_000 {
                let mut m = enc.clone();
                if !m.is_empty() {
                    let idx = (r.next() as usize) % m.len();
                    m[idx] ^= (r.next() & 0xff) as u8;
                }
                let _ = Object::decode(&m);
                let t = (r.next() as usize) % (m.len() + 1);
                let _ = Object::decode(&m[..t]);
            }
        }
    }

    #[test]
    fn fuzz_encode_decode_is_a_fixpoint() {
        let mut r = Rng(0x1234_5678_9abc_def0);
        for _ in 0..5_000 {
            let obj = random_object(&mut r);
            let enc = obj.encode();
            let dec = Object::decode(&enc).expect("a valid encoding must decode");
            assert_eq!(dec.encode(), enc, "encode∘decode must be a fixpoint");
        }
    }
}
