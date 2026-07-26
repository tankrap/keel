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
import { appendFileSync, readFileSync, mkdirSync, unlinkSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";

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

function apply(rec) {
  if (rec.op === "report") {
    sessions.set(rec.session, { repo: rec.repo, paths: new Set(rec.paths) });
  } else if (rec.op === "release") {
    sessions.delete(rec.session);
  }
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

function handle(req) {
  if (req.op === "ping") return { ok: true, pid: process.pid, workspaces: sessions.size };
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
  if (req.op === "release") {
    journal({ op: "release", session: req.session });
    return { ok: true };
  }
  if (req.op === "fleet") {
    const workspaces = [...sessions.entries()]
      .map(([session, ws]) => ({ session, repo: ws.repo, paths: ws.paths.size }))
      .sort((a, b) => (a.session < b.session ? -1 : 1));
    return { ok: true, workspaces };
  }
  return { ok: false, error: "E_USAGE", fix: "ops: ping report release fleet" };
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
      let res;
      try { res = handle(JSON.parse(line)); } catch { res = { ok: false, error: "E_PARSE" }; }
      conn.write(JSON.stringify(res) + "\n");
    }
  });
  conn.on("error", () => { /* client vanished — fine */ });
});
server.listen(SOCKET, () => {
  process.stdout.write(JSON.stringify({ ok: true, socket: SOCKET, journal: JOURNAL, pid: process.pid }) + "\n");
});
