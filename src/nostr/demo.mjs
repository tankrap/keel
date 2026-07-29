// Local roundtrip demo for the keel × nostr substrate (no network).
// Publishes a ref + provenance to an in-memory relay, fetches it back, and
// verifies BOTH signatures (nostr schnorr transport + keel Ed25519 chain).
//
//   npm i nostr-tools && node src/nostr/demo.mjs
import { makeIdentity, makeProvEvent, publishRef, fetchRef, verifyKeelEvent, signKeelClaim, relayMock, KIND_PROV } from "./keel-nostr.mjs";

const relay = relayMock();
const agent = makeIdentity();
console.log("agent nostr pubkey:", agent.nostrPub.slice(0, 16) + "…");

// 1) publish a ref update carrying a keel provenance bundle
const evt = publishRef(relay, { repo: "jharris/hi-repo", ref: "refs/heads/main", commit: "a1b2c3d4", prev: null }, agent);
console.log("\npublished ref event:");
console.log("  id      :", evt.id.slice(0, 16) + "…");
console.log("  kind    :", evt.kind, "(parameterized-replaceable ref)");
console.log("  d-tag   :", evt.tags.find((t) => t[0] === "d")[1]);

// 2) fetch it back through the relay
const got = fetchRef(relay, { repo: "jharris/hi-repo", ref: "refs/heads/main" });
console.log("\nfetched commit:", got.commit, got.commit === "a1b2c3d4" ? "✓ matches" : "✗ MISMATCH");

// 3) verify both layers of the bridge
const v = verifyKeelEvent(got.event, agent.edPub);
console.log("verify: schnorr transport sig + keel Ed25519 chain:", v.ok ? "✓ both valid" : "✗ " + v.why, "(hasProvenance=" + v.hasProvenance + ")");

// 4) latest-wins: a newer ref replaces the old one
const evt2 = publishRef(relay, { repo: "jharris/hi-repo", ref: "refs/heads/main", commit: "eeee5555", prev: "a1b2c3d4" }, agent);
const got2 = fetchRef(relay, { repo: "jharris/hi-repo", ref: "refs/heads/main" });
console.log("\nafter update — fetched commit:", got2.commit, got2.commit === "eeee5555" ? "✓ latest-wins" : "✗");
console.log("relay holds", relay.req({ kinds: [31900] }).length, "ref event (replaceable collapsed to 1)");

// 5) tamper check: flip a byte in the provenance claim → keel chain must fail
const provBundle = { ...signKeelClaim(agent.edPriv, { repo: "x", commit: "deadbeef" }), ed_pub: agent.edPub.export({ type: "spki", format: "pem" }) };
provBundle.claim.commit = "tampered"; // mutate after signing
const provEvt = makeProvEvent(provBundle, agent.nostrSec);
relay.publish(provEvt);
const tv = verifyKeelEvent(provEvt, agent.edPub);
console.log("\ntampered provenance claim rejected by keel chain:", tv.ok ? "✗ NOT rejected (bug)" : "✓ rejected (" + tv.why + ")");

console.log("\nSUBSTRATE OK — refs + provenance ride nostr (open, portable); intelligence stays hosted.");
