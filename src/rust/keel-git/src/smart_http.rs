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
    let caps = if service == "git-receive-pack" {
        format!("report-status delete-refs ofs-delta {AGENT}")
    } else {
        format!("multi_ack_detailed no-done shallow object-format=sha1 {AGENT}")
    };
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

/// The `POST /git-upload-pack` response. Handles the basic (full) exchange — `NAK` + packfile — and
/// the two-round `deepen` (shallow / `--depth`) exchange: round 1 (`deepen`, no `done`) returns the
/// `shallow` boundary lines only; round 2 (`done`) returns them again followed by `NAK` + the pack.
/// (No `have` negotiation yet — a full pack is sent, correct for `clone` and a first `fetch`.)
pub fn upload_pack(store: &Store, request: &[u8]) -> io::Result<Vec<u8>> {
    let mut wants: Vec<Oid> = Vec::new();
    let mut deepen = false;
    let mut done = false;
    let mut pos = 0;
    while let Some(pkt) = pktline::read_at(request, &mut pos) {
        if let pktline::Pkt::Line(line) = pkt {
            if let Some(rest) = line.strip_prefix(b"want ") {
                let hex = &rest[..rest.len().min(40)];
                if let Ok(oid) = Oid::from_hex(hex) {
                    wants.push(oid);
                }
            } else if line.starts_with(b"deepen") {
                deepen = true;
            } else if line.starts_with(b"done") {
                done = true;
            }
        }
    }

    let oids = server::reachable(store, &wants)?;
    let present: HashSet<[u8; 20]> = oids.iter().map(|o| *o.as_bytes()).collect();

    let mut out = Vec::new();
    if deepen {
        // Shallow boundary: reachable commits whose parents aren't present (a depth-limited /
        // depth-imported mirror). Tell the client so it doesn't expect the missing parents.
        for oid in &oids {
            if let Some((Kind::Commit, payload)) = mirror::get_object(store, oid)? {
                let parents = Headed::parse(&payload).parents().unwrap_or_default();
                if parents.iter().any(|p| !present.contains(p.as_bytes())) {
                    out.extend(pktline::encode(format!("shallow {}\n", oid.to_hex()).as_bytes()));
                }
            }
        }
        out.extend_from_slice(pktline::FLUSH);
        if !done {
            return Ok(out); // round 1: shallow-info only, no pack yet
        }
    }

    // round 2 (or a non-deepen clone): NAK then the pack of everything reachable
    out.extend(pktline::encode_str("NAK\n"));
    let mut objects = Vec::with_capacity(oids.len());
    for oid in &oids {
        if let Some(obj) = mirror::get_object(store, oid)? {
            objects.push(obj);
        }
    }
    out.extend_from_slice(&server::pack_bytes(&objects));
    Ok(out)
}

/// Handle a `POST /git-receive-pack` (push): parse the ref-update commands, unpack the packfile
/// into the mirror (resolving deltas, thin bases from the store), apply the ref updates, and
/// return a report-status. The server re-bridges keel-native history afterward.
pub fn receive_pack(store: &Store, request: &[u8]) -> io::Result<Vec<u8>> {
    // 1. ref-update commands: "<old> <new> <ref>\0caps\n" ... up to the flush; the pack follows.
    let mut commands: Vec<(String, String)> = Vec::new(); // (new-oid, ref)
    let mut pos = 0;
    while let Some(pkt) = pktline::read_at(request, &mut pos) {
        match pkt {
            pktline::Pkt::Flush => break,
            pktline::Pkt::Line(line) => {
                let line = line.split(|&b| b == 0).next().unwrap_or(line); // drop caps after NUL
                let s = String::from_utf8_lossy(line);
                let mut it = s.split_whitespace();
                if let (Some(_old), Some(new), Some(r)) = (it.next(), it.next(), it.next()) {
                    commands.push((new.to_string(), r.to_string()));
                }
            }
            pktline::Pkt::Delim => {}
        }
    }

    // 2. unpack the packfile (the remainder). Empty for a delete-only push.
    let pack = &request[pos..];
    let mut unpack_ok = true;
    let mut unpack_msg = String::from("ok");
    if pack.starts_with(b"PACK") {
        let base = |oid: &Oid| mirror::get_object(store, oid).ok().flatten();
        match crate::pack::read(pack, &base) {
            Ok(objs) => {
                mirror::ingest_objects(store, &objs)?;
            }
            Err(e) => {
                unpack_ok = false;
                unpack_msg = format!("index-pack failed: {e}");
            }
        }
    }

    // 3. apply ref updates (only if the pack unpacked cleanly)
    let mut ref_results: Vec<(String, Result<(), String>)> = Vec::new();
    if unpack_ok {
        for (new, r) in &commands {
            let res = if new.bytes().all(|b| b == b'0') {
                mirror::delete_ref(store, r).map(|_| ())
            } else {
                mirror::set_ref(store, r, new)
            };
            ref_results.push((r.clone(), res.map_err(|e| e.to_string())));
        }
    }

    // A client pushes branch refs but not HEAD; a server picks a default HEAD on the first push
    // (git does this too). Without it, a clone can't check out and the bridge has no head to walk.
    if unpack_ok {
        let refs = mirror::refs(store)?;
        if !refs.iter().any(|(n, _)| n == "HEAD") {
            let heads: Vec<&String> =
                refs.iter().map(|(n, _)| n).filter(|n| n.starts_with("refs/heads/")).collect();
            let pick = heads
                .iter()
                .find(|n| **n == "refs/heads/main" || **n == "refs/heads/master")
                .or_else(|| heads.first())
                .cloned();
            if let Some(branch) = pick {
                mirror::set_ref(store, "HEAD", &format!("ref: {branch}"))?;
            }
        }
    }

    // 4. report-status
    let mut out = Vec::new();
    out.extend(pktline::encode(format!("unpack {unpack_msg}\n").as_bytes()));
    for (r, res) in &ref_results {
        let line = match res {
            Ok(()) => format!("ok {r}\n"),
            Err(e) => format!("ng {r} {e}\n"),
        };
        out.extend(pktline::encode(line.as_bytes()));
    }
    out.extend_from_slice(pktline::FLUSH);
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
    fn receive_pack_ingests_objects_and_updates_refs() {
        let dir = std::env::temp_dir().join(format!("keelgit-recv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("store")).unwrap();

        // a real blob/tree/commit trio
        let blob = b"hi\n";
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

        // build a receive-pack request: one create command + the packfile
        let pack = crate::pack::write(&[
            (Kind::Blob, blob.to_vec()),
            (Kind::Tree, tree.clone()),
            (Kind::Commit, commit.clone()),
        ]);
        let zero = "0".repeat(40);
        let mut req = pktline::encode(
            format!("{zero} {} refs/heads/main\0report-status\n", coid.to_hex()).as_bytes(),
        );
        req.extend_from_slice(pktline::FLUSH);
        req.extend_from_slice(&pack);

        let report = receive_pack(&store, &req).unwrap();
        let text = String::from_utf8_lossy(&report);
        assert!(text.contains("unpack ok"), "report: {text}");
        assert!(text.contains("ok refs/heads/main"), "report: {text}");

        // ref set (+ HEAD synthesized), objects ingested
        let refs = mirror::refs(&store).unwrap();
        assert!(refs.iter().any(|(n, v)| n == "refs/heads/main" && v == &coid.to_hex()));
        assert!(refs.iter().any(|(n, v)| n == "HEAD" && v == "ref: refs/heads/main"));
        assert_eq!(mirror::get_object(&store, &coid).unwrap().unwrap().0, Kind::Commit);
        assert_eq!(mirror::get_object(&store, &boid).unwrap().unwrap().1, blob);
        let _ = std::fs::remove_dir_all(&dir);
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
