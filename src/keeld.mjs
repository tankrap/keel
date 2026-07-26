#!/usr/bin/env node
// keeld v0 — the per-machine keel daemon (design §7). Crash-only by design:
// there is NO shutdown path; the only way it stops is dying, so recovery IS
// the boot path. All state mutations are journaled before they are acked; the
// in-memory registry is a cache rebuilt by replaying the journal.
//
//   node src/keeld.mjs [--socket path] [--journal path]
//
// Protocol: JSON-lines over a unix socket. One request per line, one response
// per line. Ops:
//   {"op":"ping"}                                → {"ok":true,"pid":N,"workspaces":N}
//   {"op":"report","session":s,"repo":r,"paths":[…]}
//                                                → {"ok":true,"overlaps":[{"path","session","repo"}]}
//   {"op":"release","session":s}                 → {"ok":true}
//   {"op":"fleet"}                               → {"ok":true,"workspaces":[…]}

import { createServer } from "node:net";
import { appendFileSync, readFileSync, writeFileSync, mkdirSync, unlinkSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { createHash, generateKeyPairSync, sign as edSign } from "node:crypto";
import { spawnSync } from "node:child_process";

const arg = (name, dflt) => {
  const i = process.argv.indexOf(name);
  return i === -1 ? dflt : process.argv[i + 1];
};

const HOME = process.env.HOME ?? "/tmp";
const SOCKET = arg("--socket", process.env.KEEL_DAEMON ?? join(HOME, ".keel", "keeld.sock"));
const JOURNAL = arg("--journal", join(dirname(SOCKET), "keeld.journal.jsonl"));
mkdirSync(dirname(SOCKET), { recursive: true });

// registry: session key → {repo, paths:Set}. Rebuilt from journal at boot —
// a kill -9 at any point loses at most an unacked request.
const sessions = new Map();
// coordination (design #2): session → { globs: [...] } reservations. A fleet
// claims regions BEFORE working so collisions are prevented, not detected late.
const reservations = new Map();

function apply(rec) {
  if (rec.op === "report") {
    const cur = sessions.get(rec.session) ?? {};
    sessions.set(rec.session, { ...cur, repo: rec.repo, paths: new Set(rec.paths) });
  } else if (rec.op === "acquire") {
    const cur = sessions.get(rec.session) ?? { paths: new Set() };
    sessions.set(rec.session, { ...cur, repo: rec.base, ws: rec.ws, branch: rec.branch, paths: cur.paths ?? new Set() });
  } else if (rec.op === "reserve") {
    reservations.set(rec.session, { globs: rec.globs, repo: rec.repo ?? "", ts: rec.ts });
  } else if (rec.op === "unreserve") {
    reservations.delete(rec.session);
  } else if (rec.op === "release") {
    sessions.delete(rec.session);
    reservations.delete(rec.session);
  }
}

// A glob normalizes to a prefix (strip trailing /**, /*, *). Two claims collide
// if either prefix contains the other (prefix overlap) — pragmatic and honest;
// exact path membership is the prefix-equality case.
const prefixOf = (glob) => glob.replace(/\/?\*+$/u, "").replace(/\/$/u, "");
function globsCollide(a, b) {
  const pa = prefixOf(a), pb = prefixOf(b);
  if (pa === "" || pb === "") return true; // a bare "*" claims everything
  return pa === pb || pa.startsWith(pb + "/") || pb.startsWith(pa + "/");
}
// conflicts between a candidate glob set and everyone else's reservations
function collisions(session, globs) {
  const out = [];
  for (const [sid, r] of reservations) {
    if (sid === session) continue;
    for (const g of globs) {
      for (const held of r.globs) {
        if (globsCollide(g, held)) out.push({ glob: g, held, session: sid });
      }
    }
  }
  return out.sort((x, y) => (x.glob + x.session < y.glob + y.session ? -1 : 1));
}

function journal(rec) {
  appendFileSync(JOURNAL, JSON.stringify(rec) + "\n"); // durable before ack
  apply(rec);
}

// boot = recovery, always (crash-only: this path runs every start)
try {
  for (const line of readFileSync(JOURNAL, "utf8").split("\n")) {
    if (!line) continue;
    try { apply(JSON.parse(line)); } catch { /* torn tail write: ignore */ }
  }
} catch { /* first boot */ }

// shared chunk cache: one fetch per hash machine-wide, verified, single-flighted.
// N agents asking for the same chunk cost one network round trip, ever.
const CACHE = join(dirname(SOCKET), "cache");
mkdirSync(CACHE, { recursive: true });
const inflight = new Map();

async function fetchChunk(url, hash) {
  const file = join(CACHE, hash);
  if (existsSync(file)) return { ok: true, path: file, cached: true };
  if (!inflight.has(hash)) {
    inflight.set(hash, (async () => {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`upstream ${res.status}`);
      const buf = Buffer.from(await res.arrayBuffer());
      if (createHash("sha256").update(buf).digest("hex") !== hash) throw new Error("hash mismatch — refusing to cache");
      writeFileSync(file, buf);
    })().finally(() => inflight.delete(hash)));
  }
  await inflight.get(hash);
  return { ok: true, path: file, cached: false };
}

// ── workspaces: shared-object-store working copies + a session credential,
// minted in ONE operation (design §7). v0 materialization is `git worktree`
// (all workspaces share the base repo's object store — the storage dedup the
// design wants; file-level reflink CoW arrives with the core store).
const WS_ROOT = join(dirname(SOCKET), "ws");
const safe = (s) => s.replace(/[^A-Za-z0-9._-]/gu, "_").slice(0, 60);
const g = (cwd, ...args) => spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8" });

const canonicalJson = (v) => {
  const s = (x) => Array.isArray(x) ? x.map(s)
    : x && typeof x === "object" ? Object.fromEntries(Object.keys(x).sort().map((k) => [k, s(x[k])])) : x;
  return JSON.stringify(s(v));
};

/** Mint a session credential under this machine's identity chain, if present. */
function mintSession(wsDir, refs, ttlS) {
  let id;
  try { id = JSON.parse(readFileSync(join(process.env.HOME ?? "/tmp", ".keel", "id", "chain.json"), "utf8")); }
  catch { return null; } // no chain on this machine — workspace works uncredentialed
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const pub = publicKey.export({ format: "der", type: "spki" }).toString("base64");
  const body = {
    v: 1, kind: "session", sub: pub,
    id: createHash("sha256").update(Buffer.from(pub, "base64")).digest("hex").slice(0, 16),
    caveats: { refs, exp: Date.now() + ttlS * 1000 },
    parent: id.machineCert
  };
  const cert = { ...body, sig: edSign(null, Buffer.from(canonicalJson(body)), id.machinePriv).toString("base64") };
  const gitDir = g(wsDir, "rev-parse", "--git-dir").stdout.trim();
  const keelDir = join(gitDir.startsWith("/") ? gitDir : join(wsDir, gitDir), "keel");
  mkdirSync(keelDir, { recursive: true });
  writeFileSync(join(keelDir, "session.json"), JSON.stringify({ cert, priv: privateKey.export({ format: "pem", type: "pkcs8" }).toString() }), { mode: 0o600 });
  return cert.id;
}

function opAcquire(req) {
  if (!req.session || !req.base) return { ok: false, error: "E_USAGE", fix: "acquire needs {session, base} (base = path to a repo)" };
  const existing = sessions.get(req.session);
  if (existing?.ws && existsSync(existing.ws)) return { ok: true, workspace: existing.ws, branch: existing.branch, existing: true };
  mkdirSync(WS_ROOT, { recursive: true });
  const ws = join(WS_ROOT, safe(req.session));
  const branch = `ws/${safe(req.session)}`;
  const r = g(req.base, "worktree", "add", "-B", branch, ws);
  if (r.status !== 0) return { ok: false, error: "E_WORKTREE", message: (r.stderr ?? "").slice(0, 200) };
  const cred = mintSession(ws, req.refs ?? ["*"], Number(req.ttl ?? 28800));
  journal({ op: "acquire", session: req.session, base: req.base, ws, branch });
  return { ok: true, workspace: ws, branch, ...(cred ? { session_cert: cred } : {}) };
}

function opRelease(req) {
  const s = sessions.get(req.session);
  if (s?.ws) {
    g(s.repo, "worktree", "remove", "--force", s.ws);
    g(s.repo, "branch", "-D", s.branch ?? "");
  }
  journal({ op: "release", session: req.session });
  return { ok: true, ...(s?.ws ? { removed: s.ws } : {}) };
}

function opReserve(req) {
  if (!req.session || !Array.isArray(req.globs) || !req.globs.length) {
    return { ok: false, error: "E_USAGE", fix: "reserve needs {session, globs:[...]}" };
  }
  const conflicts = collisions(req.session, req.globs);
  // strict by default: refuse a reservation that collides with a live one, so
  // the orchestrator routes elsewhere. --force lets an operator claim anyway.
  if (conflicts.length && !req.force) {
    return { ok: false, error: "E_RESERVED", conflicts, fix: "reserve a disjoint region, or pass force to override" };
  }
  journal({ op: "reserve", session: req.session, globs: req.globs, repo: req.repo ?? "", ts: req.ts ?? 0 });
  return { ok: true, reserved: req.globs, ...(conflicts.length ? { forced_over: conflicts } : {}) };
}

async function handle(req) {
  if (req.op === "ping") return { ok: true, pid: process.pid, workspaces: sessions.size };
  if (req.op === "acquire") return opAcquire(req);
  if (req.op === "reserve") return opReserve(req);
  if (req.op === "unreserve") { journal({ op: "unreserve", session: req.session }); return { ok: true }; }
  if (req.op === "reservations") {
    const held = [...reservations.entries()].map(([session, r]) => ({ session, globs: r.globs, repo: r.repo })).sort((a, b) => (a.session < b.session ? -1 : 1));
    return { ok: true, reservations: held };
  }
  if (req.op === "predict") {
    // collision risk for a candidate set of globs WITHOUT reserving (planning)
    if (!Array.isArray(req.globs)) return { ok: false, error: "E_USAGE", fix: "predict needs {globs:[...]}" };
    const conflicts = collisions(req.session ?? "", req.globs);
    return { ok: true, collides: conflicts.length > 0, conflicts };
  }
  if (req.op === "fetch") {
    if (!req.url || !req.hash) return { ok: false, error: "E_USAGE", fix: "fetch needs {url, hash}" };
    try { return await fetchChunk(req.url, req.hash); }
    catch (e) { return { ok: false, error: "E_FETCH", message: String(e.message ?? e).slice(0, 120) }; }
  }
  if (req.op === "report") {
    if (!req.session || !Array.isArray(req.paths)) return { ok: false, error: "E_USAGE" };
    const overlaps = [];
    for (const [sid, ws] of sessions) {
      if (sid === req.session) continue;
      for (const p of req.paths) {
        if (ws.paths.has(p)) overlaps.push({ path: p, session: sid, repo: ws.repo });
      }
    }
    journal({ op: "report", session: req.session, repo: req.repo ?? "", paths: req.paths });
    return { ok: true, overlaps };
  }
  if (req.op === "release") return opRelease(req);
  if (req.op === "fleet") {
    const workspaces = [...sessions.entries()]
      .map(([session, ws]) => ({ session, repo: ws.repo, paths: ws.paths?.size ?? 0, ...(ws.ws ? { workspace: ws.ws, branch: ws.branch } : {}) }))
      .sort((a, b) => (a.session < b.session ? -1 : 1));
    return { ok: true, workspaces };
  }
  return { ok: false, error: "E_USAGE", fix: "ops: ping report acquire release fleet fetch" };
}

// stale socket from a previous crash: remove and take over (crash-only cleanup)
if (existsSync(SOCKET)) { try { unlinkSync(SOCKET); } catch { /* race: listen will fail loudly */ } }

const server = createServer((conn) => {
  let buf = "";
  conn.on("data", (d) => {
    buf += d;
    let nl;
    while ((nl = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      let parsed;
      try { parsed = JSON.parse(line); } catch { conn.write(JSON.stringify({ ok: false, error: "E_PARSE" }) + "\n"); continue; }
      Promise.resolve(handle(parsed))
        .catch((e) => ({ ok: false, error: "E_INTERNAL", message: String(e).slice(0, 120) }))
        .then((res) => conn.write(JSON.stringify(res) + "\n"));
    }
  });
  conn.on("error", () => { /* client vanished — fine */ });
});
server.listen(SOCKET, () => {
  process.stdout.write(JSON.stringify({ ok: true, socket: SOCKET, journal: JOURNAL, pid: process.pid }) + "\n");
});
