//! git smart-HTTP protocol (v0/v1), serving side — the response bodies for the two upload-pack
//! endpoints. Transport-independent (a caller wraps these in HTTP); see `keel serve`.
//!
//! Fetch/clone flow:
//!   GET  /info/refs?service=git-upload-pack  → [`advertisement`] (ref list + capabilities)
//!   POST /git-upload-pack                    → [`upload_pack`]    (NAK + packfile)
//!
//! We advertise a deliberately small capability set and send an undeltified pack after `NAK`
//! (no side-band) — the simplest exchange the git client accepts, which keeps this correct and
//! reviewable. Incremental fetch (honoring `have` lines) and push (`receive-pack`) build on this.

use crate::{mirror, pktline, server, Headed, Kind, Oid};
use keel_store::store::Store;
use std::collections::HashSet;
use std::io;

const AGENT: &str = "agent=keel/0.1";

/// The `GET /info/refs?service=<service>` advertisement body. HEAD is listed first (the client
/// uses it to pick the default branch); the first ref line carries the capability list after a
/// NUL. An empty repo advertises a lone `capabilities^{}` line so the client still gets caps.
pub fn advertisement(store: &Store, service: &str) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend(pktline::encode(format!("# service={service}\n").as_bytes()));
    out.extend_from_slice(pktline::FLUSH);

    let mut refs = server::advertised_refs(store)?;
    // HEAD first, everything else sorted.
    refs.sort_by(|a, b| (a.0 != "HEAD").cmp(&(b.0 != "HEAD")).then(a.0.cmp(&b.0)));

    // `multi_ack_detailed` is required by the stateless-HTTP client; `no-done` lets it skip the
    // extra round-trip. We don't advertise `side-band-64k`, so the pack is sent raw after `NAK`.
    let caps = format!("multi_ack_detailed no-done object-format=sha1 {AGENT}");
    if refs.is_empty() {
        let zero = "0".repeat(40);
        out.extend(pktline::encode(format!("{zero} capabilities^{{}}\0{caps}\n").as_bytes()));
    } else {
        for (i, (name, oid)) in refs.iter().enumerate() {
            let line = if i == 0 {
                format!("{oid} {name}\0{caps}\n")
            } else {
                format!("{oid} {name}\n")
            };
            out.extend(pktline::encode(line.as_bytes()));
        }
    }
    out.extend_from_slice(pktline::FLUSH);
    Ok(out)
}

/// The `POST /git-upload-pack` response for `request` (the client's want/have/done pkt-lines):
/// `NAK` followed by an undeltified packfile of everything reachable from the wants. (No `have`
/// negotiation yet — this always sends a full pack, correct for `clone` and a first `fetch`.)
pub fn upload_pack(store: &Store, request: &[u8]) -> io::Result<Vec<u8>> {
    let mut wants: Vec<Oid> = Vec::new();
    let mut client_shallow = false; // did the client ask for a shallow/depth clone?
    let mut pos = 0;
    while let Some(pkt) = pktline::read_at(request, &mut pos) {
        if let pktline::Pkt::Line(line) = pkt {
            if let Some(rest) = line.strip_prefix(b"want ") {
                // "want <40-hex>[ caps...]\n"
                let hex = &rest[..rest.len().min(40)];
                if let Ok(oid) = Oid::from_hex(hex) {
                    wants.push(oid);
                }
            } else if line.starts_with(b"deepen") || line.starts_with(b"shallow ") {
                client_shallow = true;
            }
        }
    }
    // Reachable object set (only objects actually present in the mirror).
    let oids = server::reachable(store, &wants)?;
    let present: HashSet<[u8; 20]> = oids.iter().map(|o| *o.as_bytes()).collect();

    // Shallow boundaries: a mirror imported at a depth (`keel clone --depth 1`) holds commits whose
    // parents aren't present. If the client asked for a shallow/depth clone, tell it which commits
    // are shallow so it doesn't expect the missing parents. We only send these when the client
    // requested shallow — an unsolicited `shallow` line breaks a normal clone (which expects
    // ACK/NAK next). So a shallow mirror is cloneable with `git clone --depth N`; a full mirror
    // serves any clone.
    let mut shallows = Vec::new();
    if client_shallow {
        for oid in &oids {
            if let Some((Kind::Commit, payload)) = mirror::get_object(store, oid)? {
                let parents = Headed::parse(&payload).parents().unwrap_or_default();
                if parents.iter().any(|p| !present.contains(p.as_bytes())) {
                    shallows.push(*oid);
                }
            }
        }
    }

    let mut out = Vec::new();
    for s in &shallows {
        out.extend(pktline::encode(format!("shallow {}\n", s.to_hex()).as_bytes()));
    }
    if !shallows.is_empty() {
        out.extend_from_slice(pktline::FLUSH);
    }
    out.extend(pktline::encode_str("NAK\n"));
    let mut objects = Vec::with_capacity(oids.len());
    for oid in &oids {
        if let Some(obj) = mirror::get_object(store, oid)? {
            objects.push(obj);
        }
    }
    out.extend_from_slice(&crate::pack::write(&objects));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash, mirror, Kind};

    fn record(kind: Kind, payload: &[u8]) -> Vec<u8> {
        let oid = hash(kind, payload);
        let mut v = format!("{} {} {}\n", oid.to_hex(), kind.as_str(), payload.len()).into_bytes();
        v.extend_from_slice(payload);
        v.push(b'\n');
        v
    }

    #[test]
    fn advertisement_lists_head_first_with_caps() {
        let dir = std::env::temp_dir().join(format!("keelgit-adv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("store")).unwrap();

        let blob = b"x\n";
        let boid = hash(Kind::Blob, blob);
        let mut tree = Vec::new();
        tree.extend_from_slice(b"100644 f\0");
        tree.extend_from_slice(boid.as_bytes());
        let toid = hash(Kind::Tree, &tree);
        let commit = format!(
            "tree {}\nauthor a <a@e> 1 +0000\ncommitter a <a@e> 1 +0000\n\nhi\n",
            toid.to_hex()
        )
        .into_bytes();
        let coid = hash(Kind::Commit, &commit);
        let mut stream = Vec::new();
        for r in [record(Kind::Blob, blob), record(Kind::Tree, &tree), record(Kind::Commit, &commit)] {
            stream.extend(r);
        }
        mirror::ingest_batch_stream(&store, &stream).unwrap();
        mirror::ingest_refs(&store, &[
            ("refs/heads/main".into(), coid.to_hex()),
            ("HEAD".into(), "ref: refs/heads/main".into()),
        ])
        .unwrap();

        let adv = advertisement(&store, "git-upload-pack").unwrap();
        let text = String::from_utf8_lossy(&adv);
        assert!(text.contains("# service=git-upload-pack"));
        assert!(text.contains("HEAD\0"), "HEAD advertised first with caps");
        assert!(text.contains(&coid.to_hex()), "commit oid advertised");
        assert!(text.contains("object-format=sha1"));

        // upload-pack for that commit returns NAK + a valid PACK
        let mut req = Vec::new();
        req.extend(pktline::encode(format!("want {}\n", coid.to_hex()).as_bytes()));
        req.extend_from_slice(pktline::FLUSH);
        req.extend(pktline::encode_str("done\n"));
        let resp = upload_pack(&store, &req).unwrap();
        assert!(resp.starts_with(b"0008NAK\n"), "response starts with NAK pkt-line");
        assert!(resp[8..].starts_with(b"PACK"), "packfile follows NAK");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
