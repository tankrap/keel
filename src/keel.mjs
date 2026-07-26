#!/usr/bin/env node
// keel v0 — agent-first VCS porcelain. Backend: git (jj, then core: decisions/0001).
// Contract: piped stdout is stable-key JSON; errors are {error,message,fix}+exit 1;
// no prompts, no pagers, byte-stable ordering. See src/design.md §3–§4.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync, appendFileSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";

const TTY = process.stdout.isTTY === true;
const argv = process.argv.slice(2);

// ── plumbing ────────────────────────────────────────────────────────────────

function git(args, input) {
  const r = spawnSync("git", args, { encoding: "utf8", input, maxBuffer: 64 * 1024 * 1024 });
  return { code: r.status ?? 1, out: (r.stdout ?? "").replace(/\n+$/u, ""), err: (r.stderr ?? "").trim() };
}

function sortKeys(v) {
  if (Array.isArray(v)) return v.map(sortKeys);
  if (v && typeof v === "object") {
    const o = {};
    for (const k of Object.keys(v).sort()) o[k] = sortKeys(v[k]);
    return o;
  }
  return v;
}

const estTokens = (s) => Math.ceil(s.length / 4);

function emit(obj, exit = 0) {
  const s = JSON.stringify(sortKeys(obj), null, TTY ? 1 : 0);
  process.stdout.write(s + "\n");
  process.exit(exit);
}

function die(code, message, fix) {
  emit({ error: code, message, ...(fix ? { fix } : {}) }, 1);
}

function requireRepo() {
  const r = git(["rev-parse", "--git-dir"]);
  if (r.code !== 0) die("E_NO_REPO", "not inside a repository", "cd into one, or: git init");
  return r.out;
}

function keelDir() {
  const d = join(requireRepo(), "keel");
  mkdirSync(d, { recursive: true });
  return d;
}

function head() {
  const r = git(["rev-parse", "--short", "HEAD"]);
  return r.code === 0 ? r.out : "";
}

const EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const diffBase = () => (head() ? "HEAD" : EMPTY_TREE);

function flag(name) {
  const i = argv.indexOf(name);
  if (i === -1) return undefined;
  argv.splice(i, 1);
  return true;
}

function opt(name, dflt) {
  const i = argv.indexOf(name);
  if (i === -1) return dflt;
  const v = argv[i + 1];
  argv.splice(i, 2);
  return v;
}

// ── st ──────────────────────────────────────────────────────────────────────

function statusFiles() {
  const r = git(["status", "--porcelain=v2", "--branch", "--untracked-files=all"]);
  const info = { branch: "", upstream: "", ahead: 0, behind: 0 };
  const files = [];
  const conflicts = [];
  for (const line of r.out.split("\n")) {
    if (line.startsWith("# branch.head ")) info.branch = line.slice(14);
    else if (line.startsWith("# branch.upstream ")) info.upstream = line.slice(18);
    else if (line.startsWith("# branch.ab ")) {
      const m = line.match(/\+(\d+) -(\d+)/u);
      if (m) { info.ahead = Number(m[1]); info.behind = Number(m[2]); }
    } else if (line.startsWith("1 ") || line.startsWith("2 ")) {
      const parts = line.split(" ");
      const xy = parts[1];
      const path = line.startsWith("2 ") ? parts.slice(9).join(" ").split("\t").pop() : parts.slice(8).join(" ");
      files.push([xy.replace(/\./gu, ""), path]);
    } else if (line.startsWith("u ")) {
      conflicts.push(line.split(" ").slice(10).join(" "));
    } else if (line.startsWith("? ")) {
      files.push(["A?", line.slice(2)]);
    }
  }
  return { info, files, conflicts };
}

function numstat() {
  const r = git(["diff", "--numstat", diffBase()]);
  const by = new Map();
  for (const line of r.out.split("\n")) {
    const m = line.match(/^(\d+|-)\t(\d+|-)\t(.+)$/u);
    if (m) by.set(m[3], [m[1] === "-" ? 0 : Number(m[1]), m[2] === "-" ? 0 : Number(m[2])]);
  }
  return by;
}

function cmdSt() {
  const { info, files, conflicts } = statusFiles();
  const ns = numstat();
  const rows = files.map(([s, p]) => {
    const [a, d] = ns.get(p) ?? (s === "A?" ? [lineCount(p), 0] : [0, 0]);
    return [s, p, a, d];
  }).sort((x, y) => (x[1] < y[1] ? -1 : 1));
  const out = {
    branch: info.branch,
    ...(info.upstream ? { upstream: info.upstream, ahead: info.ahead, behind: info.behind } : {}),
    ...(conflicts.length ? { conflicts: conflicts.sort() } : {}),
    cols: ["s", "p", "+", "-"],
    files: rows
  };
  // cursor fast-path: identical situation since last look costs one byte
  const digest = createHash("sha256").update(JSON.stringify(out)).digest("hex");
  const cur = join(keelDir(), "cursor-st");
  if (!flag("--no-cursor") && existsSync(cur) && readFileSync(cur, "utf8") === digest) {
    process.stdout.write("=\n");
    process.exit(0);
  }
  writeFileSync(cur, digest);
  emit(out);
}

function lineCount(p) {
  try { return readFileSync(p, "utf8").split("\n").length - 1; } catch { return 0; }
}

// ── d ───────────────────────────────────────────────────────────────────────

function cmdD() {
  requireRepo();
  const budget = Number(opt("--budget", process.env.KEEL_BUDGET ?? "2000"));
  const usage = flag("--usage");
  const full = flag("--full");
  const paths = argv.slice(1);
  const budgetChars = budget * 4;

  const patch = git(["diff", "--no-color", diffBase(), "--", ...paths]).out;
  const untracked = git(["ls-files", "--others", "--exclude-standard", ...(paths.length ? ["--", ...paths] : [])])
    .out.split("\n").filter(Boolean);

  // digest: per file — counts + changed function contexts from hunk headers
  const filesMap = new Map();
  let cur = null;
  for (const line of patch.split("\n")) {
    const f = line.match(/^diff --git a\/.* b\/(.*)$/u);
    if (f) { cur = { fns: new Set() }; filesMap.set(f[1], cur); continue; }
    const h = cur && line.match(/^@@ .* @@ (.*)$/u);
    if (h && h[1]) cur.fns.add(h[1].trim().slice(0, 60));
  }
  const ns = numstat();
  const rows = [...filesMap.keys()].sort().map((p) => {
    const [a, d] = ns.get(p) ?? [0, 0];
    return [p, a, d, [...filesMap.get(p).fns].sort().join(" ")];
  });
  for (const p of untracked.sort()) rows.push([p, lineCount(p), 0, "(new)"]);

  const out = { cols: ["p", "+", "-", "fns"], files: rows };

  if (full || paths.length) {
    // hunks requested: spend the budget on patch text, elide explicitly past it
    const perFile = patch.length ? patch.split(/^(?=diff --git )/mu).filter((c) => c.startsWith("diff --git ")) : [];
    const patches = [];
    const elided = [];
    let spent = JSON.stringify(out).length;
    for (const chunk of perFile) {
      const name = (chunk.match(/^diff --git a\/.* b\/(.*)$/mu) ?? [])[1] ?? "?";
      if (spent + chunk.length <= budgetChars) { patches.push({ p: name, patch: chunk.trimEnd() }); spent += chunk.length; }
      else elided.push(name);
    }
    out.patches = patches;
    if (elided.length) out.elided = { files: elided, expand: `d ${elided[0]} --budget ${budget * 4}` };
  } else if (JSON.stringify(out).length > budgetChars && rows.length > 3) {
    const keep = rows.slice(0, Math.max(3, Math.floor(rows.length / 4)));
    out.files = keep;
    out.elided = { count: rows.length - keep.length, expand: `d --budget ${budget * 4}` };
  }

  if (usage) {
    const fullDump = patch.length + untracked.map(lineCount).reduce((a, b) => a + b * 30, 0);
    out.usage = { out_est: estTokens(JSON.stringify(out)), full_dump_est: estTokens("x".repeat(fullDump)) };
  }
  emit(out);
}

// ── save / undo ─────────────────────────────────────────────────────────────

function oplogPath() { return join(keelDir(), "oplog.jsonl"); }

function cmdSave() {
  requireRepo();
  const msg = argv[1];
  if (!msg) die("E_USAGE", "save needs a message", 'save "what changed"');
  const before = head();
  git(["add", "-A"]);
  if (git(["diff", "--cached", "--quiet"]).code === 0 && before) emit({ id: before, noop: true });
  const c = git(["commit", "-q", "-m", msg]);
  if (c.code !== 0) die("E_SAVE", c.err.slice(0, 200), "check repo state with: st");
  const after = head();
  appendFileSync(oplogPath(), JSON.stringify({ op: "save", before, after }) + "\n");
  emit({ id: after });
}

function cmdUndo() {
  requireRepo();
  let entries = [];
  try { entries = readFileSync(oplogPath(), "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l)); } catch { /* empty */ }
  const last = entries.pop();
  if (!last) die("E_NOTHING", "no keel operations recorded to undo");
  if (last.after !== head()) die("E_STALE", "HEAD moved since the last keel operation; refusing", "inspect with: log");
  if (!last.before) die("E_UNSUPPORTED", "cannot undo the root commit");
  const r = git(["reset", "--soft", last.before]);
  if (r.code !== 0) die("E_UNDO", r.err.slice(0, 200));
  writeFileSync(oplogPath(), entries.map((e) => JSON.stringify(e) + "\n").join(""));
  emit({ undone: last.op, head: last.before });
}

// ── sync / fix ──────────────────────────────────────────────────────────────

function cmdSync() {
  requireRepo();
  const rebase = flag("--rebase");
  const up = git(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
  if (up.code !== 0) {
    die("E_NO_UPSTREAM", "this branch has no upstream to sync with",
      "set one: git branch --set-upstream-to=<remote>/<branch>");
  }
  const remote = up.out.split("/")[0];
  const f = git(["fetch", "--quiet", remote]);
  if (f.code !== 0) die("E_FETCH", f.err.slice(0, 200), "check the remote/network, then retry: sync");
  const c = git(["rev-list", "--left-right", "--count", `HEAD...${up.out}`]);
  const [ahead, behind] = c.out.split("\t").map(Number);
  if (behind > 0) {
    if (git(["diff", "--quiet"]).code !== 0 || git(["diff", "--cached", "--quiet"]).code !== 0) {
      die("E_DIRTY", "working tree has unsaved changes; syncing would clobber them", 'save "wip" first, then: sync');
    }
    if (ahead === 0) {
      const m = git(["merge", "--ff-only", "--quiet", up.out]);
      if (m.code !== 0) die("E_SYNC", m.err.slice(0, 200));
    } else if (!rebase) {
      die("E_DIVERGED", `local is ${ahead} ahead and ${behind} behind ${up.out}`, "sync --rebase");
    } else {
      const r = git(["-c", "core.editor=true", "rebase", up.out]);
      if (r.code !== 0) {
        const files = git(["diff", "--name-only", "--diff-filter=U"]).out.split("\n").filter(Boolean).sort();
        die("E_CONFLICT", `rebase onto ${up.out} hit ${files.length} conflict(s): ${files.join(" ")}`,
          "edit the files, then: fix --continue  (or: fix --abort)");
      }
    }
  }
  let pushed = 0;
  if (ahead > 0) {
    const p = git(["push", "--quiet", remote, "HEAD"]);
    if (p.code !== 0) die("E_PUSH", p.err.slice(0, 300), "sync again (upstream may have moved)");
    pushed = ahead;
  }
  emit({ synced: true, pulled: behind, pushed });
}

function cmdFix() {
  requireRepo();
  if (flag("--continue")) {
    const r = git(["-c", "core.editor=true", "rebase", "--continue"]);
    if (r.code !== 0) {
      const files = git(["diff", "--name-only", "--diff-filter=U"]).out.split("\n").filter(Boolean).sort();
      die("E_CONFLICT", `still conflicted: ${files.join(" ")}`, "edit the files, then: fix --continue");
    }
    emit({ fixed: true });
  }
  if (flag("--abort")) {
    git(["rebase", "--abort"]);
    emit({ aborted: true });
  }
  const files = git(["diff", "--name-only", "--diff-filter=U"]).out.split("\n").filter(Boolean).sort();
  emit({ conflicts: files });
}

// ── log ─────────────────────────────────────────────────────────────────────

function cmdLog() {
  requireRepo();
  const n = Number(opt("-n", "10"));
  const grep = opt("--grep");
  const range = argv[1];
  const args = ["log", `-n${n}`, "--format=%h\t%s"];
  if (grep) args.push(`--grep=${grep}`);
  if (range) args.push(range);
  const r = git(args);
  if (r.code !== 0) die("E_LOG", r.err.slice(0, 200));
  const commits = r.out ? r.out.split("\n").map((l) => { const i = l.indexOf("\t"); return [l.slice(0, i), l.slice(i + 1)]; }) : [];
  emit({ cols: ["id", "s"], commits });
}

// ── help / dispatch ─────────────────────────────────────────────────────────

const HELP = `keel — agent-first VCS porcelain (v0, git backend)
Output is JSON when piped. Errors: {error,message,fix} + exit 1. Never interactive.

  st                     whole situation in one call; prints = when unchanged since last look
  d [path…]              digest diff (+/-, changed fns); path or --full = hunks; --budget N tokens; --usage
  save "msg"             snapshot everything + describe; returns {id}
  sync [--rebase]        pull + push; diverged/conflicts come back as structured errors
  fix [--continue|--abort]  list conflicts / resume / abort
  log [range] [-n N] [--grep P]   compact history
  undo                   revert the last keel operation
`;

const cmds = { st: cmdSt, d: cmdD, save: cmdSave, sync: cmdSync, fix: cmdFix, log: cmdLog, undo: cmdUndo };
const cmd = argv[0];
if (!cmd || cmd === "help" || cmd === "--help") { process.stdout.write(HELP); process.exit(0); }
if (!cmds[cmd]) die("E_USAGE", `unknown command: ${cmd}`, "run: keel help");
cmds[cmd]();
