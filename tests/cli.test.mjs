import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, appendFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const KEEL = new URL("../src/keel.mjs", import.meta.url).pathname;

function keel(cwd, ...args) {
  const env = { ...process.env };
  let i;
  while ((i = args.findIndex((a) => typeof a === "object")) !== -1) Object.assign(env, args.splice(i, 1)[0]);
  try {
    const out = execFileSync(process.execPath, [KEEL, ...args], { cwd, encoding: "utf8", env });
    return { code: 0, out: out.trim() };
  } catch (e) {
    return { code: e.status ?? 1, out: (e.stdout ?? "").trim() };
  }
}
const j = (r) => JSON.parse(r.out);

function repo() {
  const dir = mkdtempSync(join(tmpdir(), "keel-test-"));
  const g = (...a) => execFileSync("git", a, { cwd: dir, encoding: "utf8" });
  g("init", "-q", "-b", "main");
  g("config", "user.email", "t@t.test");
  g("config", "user.name", "t");
  return { dir, g };
}

test("outside a repo: structured error with fix", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-norepo-"));
  const r = keel(dir, "st");
  assert.equal(r.code, 1);
  const o = j(r);
  assert.equal(o.error, "E_NO_REPO");
  assert.ok(o.fix);
});

test("save requires a message", () => {
  const { dir } = repo();
  const r = keel(dir, "save");
  assert.equal(j(r).error, "E_USAGE");
});

test("save → st → cursor fast-path", () => {
  const { dir } = repo();
  writeFileSync(join(dir, "a.txt"), "one\n");
  const s = keel(dir, "save", "first");
  assert.equal(s.code, 0);
  assert.match(j(s).id, /^[0-9a-f]{4,}$/u);

  const st1 = keel(dir, "st");
  const o = j(st1);
  assert.equal(o.branch, "main");
  assert.deepEqual(o.files, []);

  const st2 = keel(dir, "st");
  assert.equal(st2.out, "=", "unchanged situation must cost one byte");

  writeFileSync(join(dir, "a.txt"), "one\ntwo\n");
  const st3 = keel(dir, "st");
  assert.equal(j(st3).files.length, 1, "change must break the cursor fast-path");
});

test("d: digest, hunks on request, explicit elision under budget", () => {
  const { dir } = repo();
  writeFileSync(join(dir, "a.js"), "function alpha() {\n  return 1;\n}\n");
  keel(dir, "save", "base");
  appendFileSync(join(dir, "a.js"), "function beta() {\n  return 2;\n}\n");
  writeFileSync(join(dir, "b.js"), "let x = 1;\n");

  const d = j(keel(dir, "d"));
  assert.deepEqual(d.cols, ["p", "+", "-", "fns"]);
  const paths = d.files.map((f) => f[0]);
  assert.ok(paths.includes("a.js") && paths.includes("b.js"));
  assert.ok(!d.patches, "digest by default, no hunks");

  const tiny = j(keel(dir, "d", "--full", "--budget", "20"));
  assert.ok(tiny.elided && tiny.elided.expand, "over-budget elision must be explicit and expandable");

  const full = j(keel(dir, "d", "--full", "--budget", "5000"));
  assert.ok(full.patches.some((p) => p.patch.includes("function beta")));
  assert.ok(full.files.some((f) => f[3].includes("beta:new")), "semantic digest must tag new functions");

  // shown-cursor: re-requesting unchanged content returns a marker, not a re-send
  const again = j(keel(dir, "d", "--full", "--budget", "5000"));
  assert.ok(again.seen && again.seen.files.includes("a.js"), "unchanged-since-shown must not re-send");
  assert.equal(again.patches.length, 0);
  const reshow = j(keel(dir, "d", "--full", "--budget", "5000", "--reshow"));
  assert.ok(reshow.patches.some((p) => p.p === "a.js"), "--reshow must resend");

  const usage = j(keel(dir, "d", "--usage"));
  assert.ok(usage.usage.out_est > 0 && usage.usage.full_dump_est >= usage.usage.out_est);
});

test("log: compact records", () => {
  const { dir } = repo();
  writeFileSync(join(dir, "a.txt"), "x\n");
  keel(dir, "save", "first");
  writeFileSync(join(dir, "a.txt"), "y\n");
  keel(dir, "save", "second");
  const l = j(keel(dir, "log", "-n", "5"));
  assert.deepEqual(l.cols, ["id", "s"]);
  assert.equal(l.commits.length, 2);
  assert.equal(l.commits[0][1], "second");
});

test("undo reverts the last save and refuses when stale", () => {
  const { dir, g } = repo();
  writeFileSync(join(dir, "a.txt"), "x\n");
  keel(dir, "save", "first");
  writeFileSync(join(dir, "a.txt"), "y\n");
  const before = j(keel(dir, "log", "-n", "1")).commits[0][0];
  keel(dir, "save", "second");
  const u = j(keel(dir, "undo"));
  assert.equal(u.undone, "save");
  assert.equal(u.head, before);
  // HEAD moved outside keel → refuse
  writeFileSync(join(dir, "a.txt"), "z\n");
  g("add", "-A");
  g("commit", "-q", "-m", "hand-made");
  const r = keel(dir, "undo");
  assert.equal(j(r).error, "E_STALE");
});

test("profile: precedence and sources are visible", () => {
  const { dir } = repo();
  const base = j(keel(dir, "profile"));
  assert.deepEqual(base.cols, ["k", "v", "src"]);
  const get = (o, k) => o.profile.find((r) => r[0] === k);
  assert.deepEqual(get(base, "budget"), ["budget", 2000, "default"]);

  const preset = j(keel(dir, "profile", { KEEL_PROFILE: "agent" }));
  assert.deepEqual(get(preset, "budget"), ["budget", 2000, "preset:agent"]);
  assert.deepEqual(get(preset, "render"), ["render", "json", "preset:agent"]);

  const env = j(keel(dir, "profile", { KEEL_PROFILE: "agent", KEEL_BUDGET: "500" }));
  assert.deepEqual(get(env, "budget"), ["budget", 500, "env:KEEL_BUDGET"], "env must beat preset");
});

test("metrics: usage accumulates with displaced counterfactual", () => {
  const { dir } = repo();
  writeFileSync(join(dir, "a.js"), "function alpha() {\n  return 1;\n}\n");
  keel(dir, "save", "base");
  appendFileSync(join(dir, "a.js"), "function beta() {\n  return 2;\n}\n");
  keel(dir, "st");
  keel(dir, "d");
  keel(dir, "d");
  const m = j(keel(dir, "metrics"));
  const verb = Object.fromEntries(m.verbs.map((r) => [r[0], r]));
  assert.equal(verb.d[1], 2, "two d calls recorded");
  assert.ok(verb.st, "st recorded");
  assert.ok(m.totals.tokens_out > 0);
  assert.ok(verb.d[3] >= 0, "displaced tracked for d");
});

test("sync without upstream: structured error", () => {
  const { dir } = repo();
  writeFileSync(join(dir, "a.txt"), "x\n");
  keel(dir, "save", "first");
  const r = keel(dir, "sync");
  assert.equal(j(r).error, "E_NO_UPSTREAM");
});

test("link/push/pull: signed, chunk-verified sync through keel-server", async (t) => {
  const SERVER = new URL("../../keel-server/src/server.mjs", import.meta.url).pathname;
  const { existsSync } = await import("node:fs");
  if (!existsSync(SERVER)) return t.skip("keel-server checkout not present");
  const { spawn } = await import("node:child_process");
  const store = mkdtempSync(join(tmpdir(), "keel-srv-store-"));
  const child = spawn(process.execPath, [SERVER, "--port", "0", "--store", store], { stdio: ["ignore", "pipe", "inherit"] });
  t.after(() => { try { process.kill(child.pid, "SIGKILL"); } catch { /* gone */ } });
  const port = await new Promise((resolve, reject) => {
    child.stdout.once("data", (d) => resolve(JSON.parse(String(d)).port));
    setTimeout(() => reject(new Error("server boot timeout")), 5000);
  });
  const url = `http://127.0.0.1:${port}`;

  const { dir } = repo();
  writeFileSync(join(dir, "a.txt"), "one\n");
  keel(dir, "save", "first");
  const link = j(keel(dir, "link", url, "proj"));
  assert.match(link.machine, /^[0-9a-f]{16}$/u);
  const push = j(keel(dir, "push"));
  assert.equal(push.pushed, true);
  assert.equal(push.by, link.machine, "push must be attributed to this machine");
  assert.equal(j(keel(dir, "push")).current, true, "re-push is a cheap no-op");

  // an unrelated second repo pulls the same history via the server
  const { dir: dir2 } = repo();
  writeFileSync(join(dir2, "seed.txt"), "seed\n");
  keel(dir2, "save", "seed");
  j(keel(dir2, "link", url, "proj"));
  const pull = keel(dir2, "pull");
  assert.equal(j(pull).error, "E_DIVERGED", "unrelated history must not silently merge");

  // a clone of the origin fast-forwards cleanly
  const clone = mkdtempSync(join(tmpdir(), "keel-pull-"));
  execFileSync("git", ["clone", "-q", dir, clone]);
  execFileSync("git", ["config", "user.email", "t@t.test"], { cwd: clone });
  execFileSync("git", ["config", "user.name", "t"], { cwd: clone });
  writeFileSync(join(dir, "a.txt"), "one\ntwo\n");
  keel(dir, "save", "second");
  j(keel(dir, "push"));
  j(keel(clone, "link", url, "proj"));
  const ok = j(keel(clone, "pull"));
  assert.equal(ok.pulled, true);
  const clonedHead = execFileSync("git", ["rev-parse", "HEAD"], { cwd: clone, encoding: "utf8" }).trim();
  const originHead = execFileSync("git", ["rev-parse", "HEAD"], { cwd: dir, encoding: "utf8" }).trim();
  assert.equal(clonedHead, originHead, "pull lands on the pushed commit");
});

test("sync round-trip against a bare remote, and E_DIVERGED", () => {
  const { dir, g } = repo();
  const remote = mkdtempSync(join(tmpdir(), "keel-remote-"));
  execFileSync("git", ["init", "-q", "--bare", "-b", "main", remote]);
  writeFileSync(join(dir, "a.txt"), "x\n");
  keel(dir, "save", "first");
  g("remote", "add", "origin", remote);
  g("push", "-q", "-u", "origin", "main");

  writeFileSync(join(dir, "a.txt"), "x2\n");
  keel(dir, "save", "second");
  const s = j(keel(dir, "sync"));
  assert.equal(s.synced, true);
  assert.equal(s.pushed, 1);

  // second clone diverges
  const clone = mkdtempSync(join(tmpdir(), "keel-clone-"));
  execFileSync("git", ["clone", "-q", remote, clone]);
  execFileSync("git", ["config", "user.email", "t@t.test"], { cwd: clone });
  execFileSync("git", ["config", "user.name", "t"], { cwd: clone });
  writeFileSync(join(clone, "b.txt"), "theirs\n");
  keel(clone, "save", "their change");
  assert.equal(j(keel(clone, "sync")).synced, true);

  writeFileSync(join(dir, "c.txt"), "mine\n");
  keel(dir, "save", "my change");
  const dv = keel(dir, "sync");
  assert.equal(j(dv).error, "E_DIVERGED");
  assert.equal(j(dv).fix, "sync --rebase");
  const rb = j(keel(dir, "sync", "--rebase"));
  assert.equal(rb.synced, true);
});
