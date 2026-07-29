// keel × nostr — the DECENTRALIZED SUBSTRATE (identity + provenance + ref transport).
//
// What lives here (open, portable, no lock-in):
//   - agent identity as a nostr keypair (secp256k1/schnorr)
//   - ref updates as parameterized-replaceable events (latest-wins per repo#ref)
//   - provenance attestations carrying the keel Ed25519 chain inside a schnorr-
//     signed nostr event (the Ed25519 ↔ secp256k1 BRIDGE)
//
// What deliberately does NOT live here (stays in the hosted intelligence layer,
// the moat): deep semantic diff (L3-4), memoized CI, fleet coordination, the
// verification graph. Relays are dumb stores; the intelligence is a service.
//
// Consistency note: relay refs are last-writer-wins / eventually consistent. That
// is fine for publishing history, but is exactly why COORDINATION (reservations,
// conflict prediction) must stay on the ordered hosted authority, not on nostr.
//
// Requires: `npm i nostr-tools` (schnorr signing/verification). Ed25519 uses the
// node stdlib (keel's native identity primitive).
import { finalizeEvent, verifyEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";
import { generateKeyPairSync, sign as nodeSign, verify as nodeVerify } from "node:crypto";

// keel-specific nostr event kinds (NIP-34 "git-on-nostr" is the interop target;
// these are keel's own until aligned):
export const KIND_REF = 31900;  // parameterized-replaceable: latest ref per d-tag
export const KIND_PROV = 1900;  // provenance attestation (regular, append-only)

// ── identity: an agent gets a nostr keypair + a keel Ed25519 signing key ─────
export function makeIdentity() {
  const nostrSec = generateSecretKey();            // secp256k1 (32 bytes)
  const nostrPub = getPublicKey(nostrSec);         // hex x-only pubkey (npub payload)
  const { publicKey: edPub, privateKey: edPriv } = generateKeyPairSync("ed25519");
  return { nostrSec, nostrPub, edPub, edPriv };
}

// ── the Ed25519 ↔ secp256k1 BRIDGE ───────────────────────────────────────────
// keel authority is an Ed25519-signed claim (org→account→machine→session). We
// embed that signed bundle in the nostr event content and schnorr-sign the whole
// event with the agent's secp256k1 key. A verifier checks BOTH: the schnorr sig
// (transport authenticity on the relay) AND the keel Ed25519 chain (delegated
// authority). Two signatures, two questions: "did this relay identity say it?"
// and "was it authorized under keel's chain?".
export function signKeelClaim(edPriv, claimObj) {
  const bytes = Buffer.from(JSON.stringify(claimObj));
  const sig = nodeSign(null, bytes, edPriv).toString("base64");
  const edPubSpki = edPriv.type ? undefined : undefined; // (edPub carried separately)
  return { claim: claimObj, ed_sig: sig };
}
export function verifyKeelClaim(edPub, bundle) {
  const bytes = Buffer.from(JSON.stringify(bundle.claim));
  return nodeVerify(null, bytes, edPub, Buffer.from(bundle.ed_sig, "base64"));
}

// ── ref update: parameterized-replaceable event, latest-wins per repo#ref ─────
export function makeRefEvent({ repo, ref, commit, prev }, nostrSec, keelBundle) {
  const dTag = `${repo}#${ref}`;
  const tmpl = {
    kind: KIND_REF,
    created_at: Math.floor(Date.now() / 1000),
    tags: [["d", dTag], ["repo", repo], ["ref", ref], ...(prev ? [["prev", prev]] : [])],
    content: JSON.stringify({ commit, prev: prev ?? null, prov: keelBundle ?? null }),
  };
  return finalizeEvent(tmpl, nostrSec); // adds id (sha256) + schnorr sig + pubkey
}

// ── provenance attestation event ─────────────────────────────────────────────
export function makeProvEvent(keelBundle, nostrSec) {
  const tmpl = {
    kind: KIND_PROV,
    created_at: Math.floor(Date.now() / 1000),
    tags: [["t", "keel-provenance"]],
    content: JSON.stringify(keelBundle),
  };
  return finalizeEvent(tmpl, nostrSec);
}

// full verification of a keel event: nostr schnorr sig + (if present) keel chain
export function verifyKeelEvent(evt, edPub) {
  if (!verifyEvent(evt)) return { ok: false, why: "bad schnorr signature" };
  let body; try { body = JSON.parse(evt.content); } catch { return { ok: false, why: "bad content" }; }
  const bundle = body.prov ?? (evt.kind === KIND_PROV ? body : null);
  if (bundle && edPub && !verifyKeelClaim(edPub, bundle)) return { ok: false, why: "bad keel Ed25519 chain" };
  return { ok: true, hasProvenance: !!bundle };
}

// ── in-memory relay mock (NIP-01 REQ/EVENT/EOSE), no network ─────────────────
// Real deployment: a websocket client fanning out to multiple public relays with
// dedup by event id. This mock exercises the same publish/query semantics.
export function relayMock() {
  const events = [];
  return {
    publish(evt) {
      if (!verifyEvent(evt)) throw new Error("relay rejects unsigned/invalid event");
      // parameterized-replaceable: keep only the newest per (kind, pubkey, d-tag)
      if (evt.kind >= 30000 && evt.kind < 40000) {
        const d = evt.tags.find((t) => t[0] === "d")?.[1];
        const i = events.findIndex((e) => e.kind === evt.kind && e.pubkey === evt.pubkey && e.tags.find((t) => t[0] === "d")?.[1] === d);
        if (i >= 0) { if (events[i].created_at <= evt.created_at) events[i] = evt; return { accepted: true }; }
      }
      events.push(evt);
      return { accepted: true };
    },
    req(filter) { // {kinds?, authors?, "#d"?}
      return events.filter((e) =>
        (!filter.kinds || filter.kinds.includes(e.kind)) &&
        (!filter.authors || filter.authors.includes(e.pubkey)) &&
        (!filter["#d"] || filter["#d"].includes(e.tags.find((t) => t[0] === "d")?.[1])))
        .sort((a, b) => b.created_at - a.created_at);
    },
  };
}

// ── convenience: publish/fetch a ref through the substrate ───────────────────
export function publishRef(relay, { repo, ref, commit, prev }, id) {
  const bundle = { ...signKeelClaim(id.edPriv, { repo, ref, commit, by: id.nostrPub }), ed_pub: id.edPub.export({ type: "spki", format: "pem" }) };
  const evt = makeRefEvent({ repo, ref, commit, prev }, id.nostrSec, bundle);
  relay.publish(evt);
  return evt;
}
export function fetchRef(relay, { repo, ref }) {
  const [evt] = relay.req({ kinds: [KIND_REF], "#d": [`${repo}#${ref}`] });
  if (!evt) return null;
  return { commit: JSON.parse(evt.content).commit, event: evt };
}
