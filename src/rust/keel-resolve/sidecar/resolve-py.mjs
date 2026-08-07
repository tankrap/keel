// keel resolver sidecar (Python), tree-sitter backed.
//
// Same newline-JSON protocol as resolve.mjs / resolve-c.mjs. Ops: health / imports / slice /
// targets. Imports are a cheap line scan (fast at whole-tree scale, no parse); slicing uses
// tree-sitter-python for exact function/def + call extraction. Python resolves definitions
// across modules at import time, so a cross-file slice follows the import graph and, failing
// that, a repo-wide definition index (name → defining files), built once and cached.

import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

let parser = null;
let TS_ERR = null;
try {
  const require = createRequire(import.meta.url);
  const Parser = require("tree-sitter");
  const Py = require("tree-sitter-python");
  parser = new Parser();
  parser.setLanguage(Py);
} catch (e) {
  TS_ERR = String((e && e.message) || e);
}

const INDEX_FILE_CAP = 40000;

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
      respond({ id, ok: true, result: { lang: "py", version: "0.2", treeSitter: !TS_ERR } });
      return;
    }
    if (req.op === "imports") {
      respond({ id, ok: true, result: { targets: resolveImports(req.dir, req.file) } });
      return;
    }
    if (TS_ERR) throw new Error(`tree-sitter-python not available: ${TS_ERR}`);
    if (req.op === "slice") {
      respond({ id, ok: true, result: { defs: doSlice(req.dir, req.file, req.symbol, req.depth ?? 2) } });
    } else if (req.op === "targets") {
      respond({ id, ok: true, result: { targets: discoverTargets(req.dir, req.limit ?? 20) } });
    } else if (req.op === "symbols") {
      respond({ id, ok: true, result: { symbols: collectSymbols(req.dir, req.file) } });
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

// ── import graph (cheap line scan) ────────────────────────────────────────────
//
// `import a.b.c` and `from [.[.]]pkg.mod import x`. Absolute modules resolve from the repo
// root; relative (leading-dot) modules from the file's package walked up (level-1) dirs.
// Only in-repo resolutions are kept (stdlib / site-packages resolve nowhere in-repo → dropped).
const IMPORT_RE = /^[ \t]*(?:import[ \t]+([\w.]+)|from[ \t]+(\.*)([\w.]*)[ \t]+import\b)/gm;

function resolveImports(dir, file) {
  const src = safeRead(path.join(dir, file));
  if (src == null) return [];
  const out = [];
  const seen = new Set();
  IMPORT_RE.lastIndex = 0;
  let m;
  while ((m = IMPORT_RE.exec(src)) !== null) {
    const resolved = m[1]
      ? resolveModule(dir, file, 0, m[1]) // import a.b.c
      : resolveModule(dir, file, m[2].length, m[3]); // from ...pkg import x
    if (resolved && !seen.has(resolved)) {
      seen.add(resolved);
      out.push(resolved);
    }
  }
  return out;
}

// Resolve a (level, dotted-module) to an in-repo `.py` file or package `__init__.py`.
function resolveModule(dir, file, level, module) {
  let baseAbs;
  if (level === 0) {
    baseAbs = dir; // absolute import — from the repo root
  } else {
    // relative — from the file's package dir, up (level-1) parents
    baseAbs = path.dirname(path.join(dir, file));
    for (let i = 1; i < level; i++) baseAbs = path.dirname(baseAbs);
  }
  const parts = module ? module.split(".") : [];
  const stem = path.join(baseAbs, ...parts);
  for (const cand of [`${stem}.py`, path.join(stem, "__init__.py")]) {
    try {
      if (fs.statSync(cand).isFile()) return path.relative(dir, cand);
    } catch {
      /* not this candidate */
    }
  }
  return null;
}

// ── function definitions & calls (tree-sitter) ────────────────────────────────

function walk(node, visit) {
  visit(node);
  for (const c of node.namedChildren) walk(c, visit);
}

function defName(fdef) {
  const n = fdef.childForFieldName("name");
  return n && n.type === "identifier" ? n.text : null;
}

/// All `def`s in a tree (module-level and methods): [{ name, node }].
function funcDefs(tree) {
  const out = [];
  walk(tree.rootNode, (n) => {
    if (n.type === "function_definition") {
      const name = defName(n);
      if (name) out.push({ name, node: n });
    }
  });
  return out;
}

/// Identifier callees within `node` (`call` whose function is a bare name, or the final
/// attribute of `obj.method(...)`), excluding `self`.
function calleesOf(node, self) {
  const out = new Set();
  walk(node, (n) => {
    if (n.type !== "call") return;
    const fn = n.childForFieldName("function");
    if (!fn) return;
    let name = null;
    if (fn.type === "identifier") name = fn.text;
    else if (fn.type === "attribute") {
      const attr = fn.childForFieldName("attribute");
      if (attr && attr.type === "identifier") name = attr.text;
    }
    if (name && name !== self) out.add(name);
  });
  return [...out];
}

// ── cross-file slice ──────────────────────────────────────────────────────────

function doSlice(dir, file, symbol, depth, max = 40) {
  const cache = new Map();
  const load = (rel) => {
    if (cache.has(rel)) return cache.get(rel);
    const src = safeRead(path.join(dir, rel));
    let entry = null;
    if (src != null) {
      const defs = new Map();
      for (const { name, node } of funcDefs(parser.parse(src))) if (!defs.has(name)) defs.set(name, node);
      entry = { src, defs };
    }
    cache.set(rel, entry);
    return entry;
  };
  const searchSet = (rel) => [...new Set([rel, ...resolveImports(dir, rel)])];

  const start = load(file);
  if (!start || !start.defs.has(symbol)) throw new Error(`symbol not found: ${symbol} in ${file}`);

  const chosen = new Map();
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

// ── repo-wide definition index (name → files), cached, bounded ────────────────

const indexCache = new Map();
function defIndex(dir) {
  if (indexCache.has(dir)) return indexCache.get(dir);
  const idx = new Map();
  const files = walkPy(dir, INDEX_FILE_CAP);
  if (files.length >= INDEX_FILE_CAP) {
    process.stderr.write(`resolve-py: definition index capped at ${INDEX_FILE_CAP} files\n`);
  }
  for (const abs of files) {
    const src = safeRead(abs);
    if (src == null || src.length > 800_000) continue;
    const rel = path.relative(dir, abs);
    for (const { name } of funcDefs(parser.parse(src))) {
      let arr = idx.get(name);
      if (!arr) idx.set(name, (arr = []));
      if (!arr.includes(rel)) arr.push(rel);
    }
  }
  indexCache.set(dir, idx);
  return idx;
}

function walkPy(dir, cap) {
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
      if (e.name.startsWith(".") || e.name === "node_modules" || e.name === "__pycache__") continue;
      const p = path.join(d, e.name);
      if (e.isDirectory()) go(p);
      else if (/\.py$/.test(e.name)) out.push(p);
    }
  };
  go(dir);
  return out;
}

function discoverTargets(dir, limit) {
  const out = [];
  for (const abs of walkPy(dir, 400)) {
    if (out.length >= limit) break;
    const src = safeRead(abs);
    if (src == null || src.length > 400_000) continue;
    const rel = path.relative(dir, abs);
    const imports = resolveImports(dir, rel);
    for (const { name, node } of funcDefs(parser.parse(src))) {
      if (out.length >= limit) break;
      if (node.endIndex - node.startIndex < 100) continue;
      const crossFile = calleesOf(node, name).some((c) =>
        imports.some((inc) => {
          const s = safeRead(path.join(dir, inc));
          return s != null && funcDefs(parser.parse(s)).some((f) => f.name === c);
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

// The defs in `file` with 1-based inclusive line ranges — AST-accurate symbol boundaries the
// semantic diff uses to name which def/class an added line lives in. `def`s (module-level, nested,
// and methods) and classes; tree-sitter positions are 0-based rows, so +1.
function collectSymbols(dir, file) {
  const src = safeRead(path.resolve(dir, file));
  if (src == null) throw new Error(`file not found: ${file}`);
  const tree = parser.parse(src);
  const out = [];
  const push = (n, kind) => {
    const name = defName(n); // childForFieldName("name") — works for def and class
    if (name) out.push({ name, kind, startLine: n.startPosition.row + 1, endLine: n.endPosition.row + 1 });
  };
  walk(tree.rootNode, (n) => {
    if (n.type === "function_definition") push(n, "function");
    else if (n.type === "class_definition") push(n, "class");
    else if (n.type === "decorated_definition") {
      // Also emit the decorated node's WIDER range (name/kind from the inner def) so a change to a
      // `@decorator` line above a def attributes to that def, not just its enclosing scope. The inner
      // def is still emitted above with its tighter range, so a body line resolves to it (innermost).
      const inner = n.namedChildren.find(
        (c) => c.type === "function_definition" || c.type === "class_definition"
      );
      if (inner) {
        const name = defName(inner);
        const kind = inner.type === "class_definition" ? "class" : "function";
        if (name) out.push({ name, kind, startLine: n.startPosition.row + 1, endLine: n.endPosition.row + 1 });
      }
    }
  });
  return out;
}
