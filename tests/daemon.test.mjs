import { test, after } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { connect } from "node:net";

const KEELD = new URL("../src/keeld.mjs", import.meta.url).pathname;
const KEEL = new URL("../src/keel.mjs", import.meta.url).pathname;

const dir = mkdtempSync(join(tmpdir(), "keeld-test-"));
const SOCKET = join(dir, "keeld.sock");
const kids = [];
after(() => kids.forEach((k) => { try { process.kill(k.pid, "SIGKILL"); } catch { /* gone */ } }));

function startDaemon() {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [KEELD, "--socket", SOCKET], { stdio: ["ignore", "pipe", "inherit"] });
    kids.push(child);
    child.stdout.once("data", (d) => resolve({ child, boot: JSON.parse(String(d)) }));
    child.once("exit", (c) => reject(new Error(`keeld exited ${c}`)));
    setTimeout(() => reject(new Error("keeld boot timeout")), 5000);
  });
}

function call(req) {
  return new Promise((resolve, reject) => {
    const s = connect(SOCKET);
    let buf = "";
    s.on("connect", () => s.write(JSON.stringify(req) + "\n"));
    s.on("data", (d) => {
      buf += d;
      if (buf.includes("\n")) { s.end(); resolve(JSON.parse(buf.split("\n")[0])); }
    });
    s.on("error", reject);
    setTimeout(() => reject(new Error("call timeout")), 3000);
  });
}

test("daemon: overlap detection, kill -9 recovery, CLI integration", async () => {
  const { child } = await startDaemon();

  assert.equal((await call({ op: "ping" })).ok, true);

  const a = await call({ op: "report", session: "agent-A", repo: "r1", paths: ["src/x.js", "src/y.js"] });
  assert.deepEqual(a.overlaps, [], "first reporter sees no overlap");

  const b = await call({ op: "report", session: "agent-B", repo: "r1", paths: ["src/y.js", "src/z.js"] });
  assert.equal(b.overlaps.length, 1);
  assert.equal(b.overlaps[0].path, "src/y.js");
  assert.equal(b.overlaps[0].session, "agent-A");

  // crash-only: SIGKILL, restart, state must survive via the journal
  process.kill(child.pid, "SIGKILL");
  await new Promise((r) => child.once("exit", r));
  await startDaemon();
  const fleet = await call({ op: "fleet" });
  assert.equal(fleet.workspaces.length, 2, "journal replay must restore both sessions");

  await call({ op: "release", session: "agent-B" });
  assert.equal((await call({ op: "fleet" })).workspaces.length, 1);

  // CLI: st in a repo overlapping agent-A's paths surfaces the warning
  const repo = mkdtempSync(join(tmpdir(), "keeld-repo-"));
  const g = (...args) => execFileSync("git", args, { cwd: repo, encoding: "utf8" });
  g("init", "-q", "-b", "main");
  g("config", "user.email", "t@t.test");
  g("config", "user.name", "t");
  writeFileSync(join(repo, "base.txt"), "base\n");
  g("add", "-A"); g("commit", "-q", "-m", "base");
  writeFileSync(join(repo, "src.js"), "edit\n");
  await call({ op: "report", session: "agent-C", repo: "other", paths: ["src.js"] });
  const st = JSON.parse(execFileSync(process.execPath, [KEEL, "st"], { cwd: repo, encoding: "utf8", env: { ...process.env, KEEL_DAEMON: SOCKET } }));
  assert.ok(st.overlap && st.overlap.some((o) => o[0] === "src.js" && o[1] === "agent-C"), "st must surface cross-session overlap");

  const fl = JSON.parse(execFileSync(process.execPath, [KEEL, "fleet"], { cwd: repo, encoding: "utf8", env: { ...process.env, KEEL_DAEMON: SOCKET } }));
  assert.ok(fl.workspaces.length >= 2, "fleet lists machine-wide sessions");

  // shared cache: verified fetch, second request is a cache hit, bad hash refused
  const { createServer: httpServer } = await import("node:http");
  const { createHash } = await import("node:crypto");
  const payload = Buffer.from("chunk-payload-for-cache");
  const hash = createHash("sha256").update(payload).digest("hex");
  let hits = 0;
  const upstream = httpServer((_, res) => { hits++; res.end(payload); });
  await new Promise((r) => upstream.listen(0, "127.0.0.1", r));
  const url = `http://127.0.0.1:${upstream.address().port}/c`;
  const f1 = await call({ op: "fetch", url, hash });
  assert.equal(f1.ok, true);
  assert.equal(f1.cached, false);
  const f2 = await call({ op: "fetch", url, hash });
  assert.equal(f2.cached, true, "second fetch must be a cache hit");
  assert.equal(hits, 1, "upstream must be hit exactly once");
  const bad = await call({ op: "fetch", url, hash: "0".repeat(64) });
  assert.equal(bad.ok, false, "hash mismatch must refuse to cache");
  upstream.close();
});

test("workspaces: shared store, minted credentials, isolation, crash recovery", async () => {
  // machine identity for minting: run `keel id init` under an isolated HOME
  // that the daemon ALSO uses (it reads ~/.keel/id at mint time)
  const home = mkdtempSync(join(tmpdir(), "keeld-ws-home-"));
  execFileSync(process.execPath, [KEEL, "id", "init"], { encoding: "utf8", env: { ...process.env, HOME: home } });

  const SOCK2 = join(home, "keeld.sock");
  const startD = () => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [KEELD, "--socket", SOCK2], { stdio: ["ignore", "pipe", "inherit"], env: { ...process.env, HOME: home } });
    kids.push(child);
    child.stdout.once("data", () => resolve(child));
    setTimeout(() => reject(new Error("boot timeout")), 5000);
  });
  const call2 = (req) => new Promise((resolve, reject) => {
    const s = connect(SOCK2);
    let buf = "";
    s.on("connect", () => s.write(JSON.stringify(req) + "\n"));
    s.on("data", (d) => { buf += d; if (buf.includes("\n")) { s.end(); resolve(JSON.parse(buf.split("\n")[0])); } });
    s.on("error", reject);
    setTimeout(() => reject(new Error("call timeout")), 5000);
  });
  const child = await startD();

  // base repo with one commit
  const base = mkdtempSync(join(tmpdir(), "keeld-base-"));
  const g = (...args) => execFileSync("git", args, { cwd: base, encoding: "utf8" });
  g("init", "-q", "-b", "main");
  g("config", "user.email", "t@t.test"); g("config", "user.name", "t");
  writeFileSync(join(base, "shared.js"), "export const x = 1;\n");
  g("add", "-A"); g("commit", "-q", "-m", "base");

  const a = await call2({ op: "acquire", session: "agent-A", base, refs: ["proj/*"], ttl: 600 });
  const b = await call2({ op: "acquire", session: "agent-B", base });
  assert.equal(a.ok, true); assert.equal(b.ok, true);
  assert.notEqual(a.workspace, b.workspace);
  assert.match(a.session_cert, /^[0-9a-f]{16}$/u, "acquire mints a session credential");
  assert.notEqual(a.session_cert, b.session_cert, "each workspace gets its own credential");

  // shared object store: worktrees point at the base .git, no object copies
  const gitFile = readFileSync(join(a.workspace, ".git"), "utf8");
  assert.ok(gitFile.includes(base), "workspace shares the base repo's object store");
  // isolation: A's edit is invisible in B
  writeFileSync(join(a.workspace, "shared.js"), "export const x = 2;\n");
  assert.equal(readFileSync(join(b.workspace, "shared.js"), "utf8"), "export const x = 1;\n");
  // each workspace carries its minted credential where keel push will find it
  const credPath = execFileSync("git", ["-C", a.workspace, "rev-parse", "--git-dir"], { encoding: "utf8" }).trim();
  assert.ok(readFileSync(join(credPath, "keel", "session.json"), "utf8").includes('"session"'));

  // idempotent re-acquire
  const again = await call2({ op: "acquire", session: "agent-A", base });
  assert.equal(again.existing, true);
  assert.equal(again.workspace, a.workspace);

  // crash-only: kill -9, restart, registry intact; release cleans up fully
  process.kill(child.pid, "SIGKILL");
  await new Promise((r) => child.once("exit", r));
  await startD();
  const fleet = await call2({ op: "fleet" });
  assert.equal(fleet.workspaces.filter((w) => w.workspace).length, 2, "workspaces survive kill -9 via journal");
  const rel = await call2({ op: "release", session: "agent-A" });
  assert.equal(rel.ok, true);
  assert.equal(existsSync(join(a.workspace, "shared.js")), false, "release removes the worktree");
  assert.equal((await call2({ op: "fleet" })).workspaces.filter((w) => w.workspace).length, 1);
});
