// keel resolver sidecar (C / C++), tree-sitter backed.
//
// Same newline-JSON protocol as resolve.mjs. Ops: health / imports / slice / targets.
// Parsing is exact — tree-sitter-c builds a real AST, so `#include`s, function definitions,
// and calls are read from the grammar (not regex/brace-scanning), which is robust to macros,
// comments, strings, and awkward formatting.
//
// C has no module system: the `#include` graph is the dependency graph, and a function's
// DEFINITION lives in some .c file resolved at link time (not via the header that declares
// it). So a cross-file slice consults a repo-wide definition index (name → defining files),
// built once and cached, bounded by INDEX_FILE_CAP.

import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

// tree-sitter is a native module; load it defensively so a missing install degrades to a
// clear error rather than crashing the sidecar on startup (mirrors the TS sidecar's
// optional-typescript handling).
let parser = null;
let TS_ERR = null;
try {
  const require = createRequire(import.meta.url);
  const Parser = require("tree-sitter");
  const C = require("tree-sitter-c");
  parser = new Parser();
  parser.setLanguage(C); // pass the full grammar object (carries nodeTypeInfo)
} catch (e) {
  TS_ERR = String((e && e.message) || e);
}

const INDEX_FILE_CAP = 40000;
/// Wall-clock budget for a cold repo-wide index build (ms). Bounds `keel brief` latency on very
/// large repos; a warm `keeld` isn't time-boxed (it builds once and caches).
const INDEX_TIME_BUDGET_MS = Number(process.env.KEEL_INDEX_BUDGET_MS) || 6000;

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  const id = req.id;
  try {
    if (req.op === "health") {
      respond({ id, ok: true, result: { lang: "c", version: "0.2", treeSitter: !TS_ERR } });
      return;
    }
    if (req.op === "imports") {
      // The include graph is a cheap line scan — no full parse — so it stays fast at
      // whole-tree scale AND works even if tree-sitter isn't installed.
      respond({ id, ok: true, result: { targets: resolveIncludes(req.dir, req.file) } });
      return;
    }
    // slice / targets need the AST
    if (TS_ERR) throw new Error(`tree-sitter-c not available: ${TS_ERR}`);
    if (req.op === "slice") {
      respond({ id, ok: true, result: { defs: doSlice(req.dir, req.file, req.symbol, req.depth ?? 2) } });
    } else if (req.op === "targets") {
      respond({ id, ok: true, result: { targets: discoverTargets(req.dir, req.limit ?? 20) } });
    } else {
      respond({ id, ok: false, error: `unknown op: ${req.op}` });
    }
  } catch (e) {
    respond({ id, ok: false, error: String((e && e.message) || e) });
  }
});

function respond(o) {
  process.stdout.write(JSON.stringify(o) + "\n");
}

function parse(src) {
  return parser.parse(src);
}

/// Depth-first walk of named nodes, calling `visit(node)`.
function walk(node, visit) {
  visit(node);
  for (const c of node.namedChildren) walk(c, visit);
}

// ── include graph (cheap line scan — NOT a full parse) ────────────────────────
//
// `#include "x"` / `#include <x>` are line-oriented preprocessor directives, so a line
// regex is both accurate and orders of magnitude cheaper than AST-parsing every file just
// to read its includes (full tree-sitter parsing of the kernel took >10× longer). Used by
// the `imports` op AND by the slice's include search.
const INCLUDE_RE = /^[ \t]*#[ \t]*include[ \t]*(?:"([^"]+)"|<([^>]+)>)/gm;

function resolveIncludes(dir, file) {
  const src = safeRead(path.join(dir, file));
  if (src == null) return [];
  const out = [];
  const seen = new Set();
  INCLUDE_RE.lastIndex = 0;
  let m;
  while ((m = INCLUDE_RE.exec(src)) !== null) {
    const spec = m[1] || m[2];
    const quoted = m[1] != null;
    const resolved = resolveIncludePath(dir, file, spec, quoted);
    if (resolved && !seen.has(resolved)) {
      seen.add(resolved);
      out.push(resolved);
    }
  }
  return out;
}

// Heuristic C include search: quoted includes look next to the file first; both forms then
// try `<root>/include/<spec>` (the conventional project/kernel include root) and
// `<root>/<spec>`. Real -I paths are a follow-up.
function resolveIncludePath(dir, file, spec, quoted) {
  const cands = [];
  const base = path.dirname(path.join(dir, file));
  if (quoted) cands.push(path.resolve(base, spec));
  cands.push(path.join(dir, "include", spec));
  cands.push(path.join(dir, spec));
  if (!quoted) cands.push(path.resolve(base, spec));
  for (const c of cands) {
    try {
      if (fs.statSync(c).isFile()) return path.relative(dir, c);
    } catch {
      /* not this candidate */
    }
  }
  return null;
}

// ── function definitions & calls (from the AST) ───────────────────────────────

/// The declared name of a `function_definition` node, descending through pointer/
/// parenthesized declarators to the identifier. null if it isn't a plain named function.
function funcName(fdef) {
  let d = fdef.childForFieldName("declarator");
  const outer = d;
  while (d && (d.type === "pointer_declarator" || d.type === "parenthesized_declarator")) {
    d = d.childForFieldName("declarator");
  }
  if (d && d.type === "function_declarator") {
    let id = d.childForFieldName("declarator");
    while (id && id.type !== "identifier") id = id.childForFieldName ? id.childForFieldName("declarator") : null;
    if (id && id.type === "identifier") return id.text;
  }
  // Kernel-style macro decorations (`asmlinkage __visible void __sched schedule(void)`) confuse
  // tree-sitter-c: it swallows the real name into the return `type` (a type_identifier) and reads
  // `(params)` as a parenthesized_declarator. Recover the name from the `type` field in that case
  // — otherwise every `__sched`/`asmlinkage`/`__init`-decorated function is invisible.
  if (outer && outer.type === "parenthesized_declarator") {
    const t = fdef.childForFieldName("type");
    if (t && (t.type === "type_identifier" || t.type === "identifier")) return t.text;
  }
  return null;
}

/// All function definitions in a tree: [{ name, node }].
function funcDefs(tree) {
  const out = [];
  walk(tree.rootNode, (n) => {
    if (n.type === "function_definition") {
      const name = funcName(n);
      if (name) out.push({ name, node: n });
    }
  });
  return out;
}

/// Identifier names called (`call_expression` with an identifier callee) within `node`,
/// excluding `self`.
function calleesOf(node, self) {
  const out = new Set();
  walk(node, (n) => {
    if (n.type === "call_expression") {
      const fn = n.childForFieldName("function");
      if (fn && fn.type === "identifier" && fn.text !== self) out.add(fn.text);
    }
  });
  return [...out];
}

// ── cross-file slice: target + its callees, via includes + the definition index ─

function doSlice(dir, file, symbol, depth, max = 40) {
  const cache = new Map(); // rel → { src, defs: Map(name→node) } | null
  const load = (rel) => {
    if (cache.has(rel)) return cache.get(rel);
    const src = safeRead(path.join(dir, rel));
    let entry = null;
    if (src != null) {
      const defs = new Map();
      for (const { name, node } of funcDefs(parse(src))) if (!defs.has(name)) defs.set(name, node);
      entry = { src, defs };
    }
    cache.set(rel, entry);
    return entry;
  };
  const searchSet = (rel) => [...new Set([rel, ...resolveIncludes(dir, rel)])];

  const start = load(file);
  if (!start || !start.defs.has(symbol)) throw new Error(`symbol not found: ${symbol} in ${file}`);

  const chosen = new Map(); // "rel#name" → { file, symbol, text }
  const keyOf = (f, s) => `${f}#${s}`;
  const emit = (rel, name, node) => {
    const key = keyOf(rel, name);
    if (chosen.has(key)) return false;
    chosen.set(key, { file: rel, symbol: name, text: node.text });
    return true;
  };
  emit(file, symbol, start.defs.get(symbol));

  let frontier = [{ file, node: start.defs.get(symbol), symbol }];
  for (let d = 0; d < depth && chosen.size < max; d++) {
    const next = [];
    for (const fr of frontier) {
      for (const callee of calleesOf(fr.node, fr.symbol)) {
        if (chosen.size >= max) break;
        // local include search first, then the repo-wide definition index
        let rel = null;
        for (const cand of searchSet(fr.file)) {
          const e = load(cand);
          if (e && e.defs.has(callee)) {
            rel = cand;
            break;
          }
        }
        if (!rel) {
          for (const cand of defIndex(dir).get(callee) || []) {
            const e = load(cand);
            if (e && e.defs.has(callee)) {
              rel = cand;
              break;
            }
          }
        }
        if (rel) {
          const node = load(rel).defs.get(callee);
          if (emit(rel, callee, node)) next.push({ file: rel, node, symbol: callee });
        }
      }
    }
    frontier = next;
    if (!next.length) break;
  }
  return [...chosen.values()];
}

// Control-flow / operator keywords that precede `(...) {` but aren't function names.
const NOT_A_FUNC = new Set([
  "if", "while", "for", "switch", "do", "else", "return", "sizeof", "case",
  "typeof", "__typeof__", "asm", "__asm__", "catch", "and", "or",
]);

/// Fast, AST-free extraction of likely function-definition names: an identifier followed by a
/// `(...)` argument list and then an opening `{` (whitespace/newlines allowed, so multi-line
/// kernel signatures like `... schedule(void)\n{` match). Liberal by design — used only to pick
/// candidate files for the index; the real slice re-checks with tree-sitter.
function funcNamesFast(src) {
  const names = new Set();
  const re = /([A-Za-z_]\w*)\s*\([^;{}]*\)\s*\{/g;
  let m;
  while ((m = re.exec(src)) !== null) {
    if (!NOT_A_FUNC.has(m[1])) names.add(m[1]);
  }
  return names;
}

// ── repo-wide function-definition index (name → files), cached, bounded ───────

const indexCache = new Map();
function defIndex(dir) {
  if (indexCache.has(dir)) return indexCache.get(dir);
  const files = walkSources(dir, INDEX_FILE_CAP);

  // Disk cache: the index is a pure function of the source files, so persist it under `.keel/`
  // (git-excluded) keyed by a cheap signature (file count + Σ mtime+size). A warm repo loads it
  // instead of re-scanning — this is what makes a repeat cross-file slice on a hub function like
  // the kernel's `schedule` fast on the CLI, not just under `keeld`.
  const cacheFile = path.join(dir, ".keel", "c-defindex.json");
  const sig = indexSignature(files);
  try {
    const cached = JSON.parse(fs.readFileSync(cacheFile, "utf8"));
    if (cached && cached.sig === sig && cached.idx) {
      const idx = new Map(Object.entries(cached.idx));
      indexCache.set(dir, idx);
      return idx;
    }
  } catch {
    /* no/stale/corrupt cache → rebuild below */
  }

  const idx = new Map();
  // Cold cross-file slicing on a giant repo (the kernel is ~40k source files) must stay
  // responsive, so cap the index build by TIME as well as file count — a partial index still
  // resolves most callees, and a warm `keeld` rebuilds it fully once and keeps it. The budget
  // is generous enough to fully index normal repos.
  const t0 = Date.now();
  let scanned = 0;
  for (const abs of files) {
    if (Date.now() - t0 > INDEX_TIME_BUDGET_MS) {
      process.stderr.write(
        `resolve-c: definition index time-boxed after ${scanned}/${files.length} files ` +
          `(run keeld to index fully + warm)\n`
      );
      break;
    }
    scanned++;
    const src = safeRead(abs);
    if (src == null || src.length > 800_000) continue;
    const rel = path.relative(dir, abs);
    // The index only needs name → file (which files to *consider*); the slice then loads and
    // tree-sitter-parses the chosen file to get the real node. So extract names with a fast
    // regex instead of parsing every file in the repo — parsing tens of thousands of kernel
    // files here is what made a cold cross-file slice take minutes. False positives are
    // harmless: `load()` verifies the def actually exists before emitting it.
    for (const name of funcNamesFast(src)) {
      let arr = idx.get(name);
      if (!arr) idx.set(name, (arr = []));
      if (!arr.includes(rel)) arr.push(rel);
    }
  }
  // Persist for the next brief (best-effort). Caching even a time-boxed *partial* index is
  // correct: a reload yields exactly what rebuilding would have, just without the scan. On a huge
  // repo this trades "keep re-scanning each brief" for "fast + consistent"; a warm `keeld` (no
  // time box) overwrites it with the full index. The signature invalidates it when files change.
  try {
    fs.mkdirSync(path.join(dir, ".keel"), { recursive: true });
    const obj = Object.create(null);
    for (const [k, v] of idx) obj[k] = v;
    fs.writeFileSync(cacheFile, JSON.stringify({ sig, idx: obj }));
  } catch {
    /* cache write is best-effort */
  }
  indexCache.set(dir, idx);
  return idx;
}

/// A cheap change-signature over the repo's source files: count + Σ(mtime_ms + size). A stat
/// sweep (no reads), so validating the cache is far cheaper than rebuilding it.
function indexSignature(files) {
  let n = 0;
  let sum = 0;
  for (const abs of files) {
    try {
      const s = fs.statSync(abs);
      n++;
      sum += Math.floor(s.mtimeMs) + s.size;
    } catch {
      /* unreadable → ignore */
    }
  }
  return `${n}:${sum}`;
}

function walkSources(dir, cap) {
  const out = [];
  const go = (d) => {
    if (out.length >= cap) return;
    let ents;
    try {
      ents = fs.readdirSync(d, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of ents) {
      if (out.length >= cap) return;
      if (e.name.startsWith(".") || e.name === "node_modules") continue;
      const p = path.join(d, e.name);
      if (e.isDirectory()) go(p);
      else if (/\.(c|cc|cpp|cxx|h|hh|hpp|hxx)$/.test(e.name)) out.push(p);
    }
  };
  go(dir);
  return out;
}

// ── target discovery (bench helper) ───────────────────────────────────────────

function discoverTargets(dir, limit) {
  const out = [];
  for (const abs of walkSources(dir, 400)) {
    if (out.length >= limit) break;
    const src = safeRead(abs);
    if (src == null || src.length > 400_000) continue;
    const rel = path.relative(dir, abs);
    const includes = resolveIncludes(dir, rel);
    for (const { name, node } of funcDefs(parse(src))) {
      if (out.length >= limit) break;
      if (node.endIndex - node.startIndex < 120) continue;
      const callees = calleesOf(node, name);
      const crossFile = callees.some((c) =>
        includes.some((inc) => {
          const s = safeRead(path.join(dir, inc));
          return s != null && funcDefs(parse(s)).some((f) => f.name === c);
        }),
      );
      if (crossFile) out.push({ file: rel, symbol: name });
    }
  }
  return out;
}

function safeRead(p) {
  try {
    return fs.readFileSync(p, "utf8");
  } catch {
    return null;
  }
}
