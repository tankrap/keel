// keel resolver sidecar (Go), tree-sitter backed.
//
// Same newline-JSON protocol as the other sidecars. Ops: health / imports / slice / targets.
// Go imports name PACKAGES (directories) by module path; intra-module imports resolve via the
// go.mod module path to the package dir's .go files. Slicing uses tree-sitter-go for exact
// func/method + call extraction; cross-file/package callees resolve through a repo-wide
// definition index (name → files), like the C/Python sidecars.

import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

let parser = null;
let TS_ERR = null;
try {
  const require = createRequire(import.meta.url);
  const Parser = require("tree-sitter");
  const Go = require("tree-sitter-go");
  parser = new Parser();
  parser.setLanguage(Go);
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
      respond({ id, ok: true, result: { lang: "go", version: "0.2", treeSitter: !TS_ERR } });
      return;
    }
    if (req.op === "imports") {
      respond({ id, ok: true, result: { targets: resolveImports(req.dir, req.file) } });
      return;
    }
    if (TS_ERR) throw new Error(`tree-sitter-go not available: ${TS_ERR}`);
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

// ── import graph (module-path → in-repo package dir → its .go files) ──────────

// go.mod module path (cached per dir); "" if none.
const modCache = new Map();
function modulePath(dir) {
  if (modCache.has(dir)) return modCache.get(dir);
  let m = "";
  try {
    const gomod = fs.readFileSync(path.join(dir, "go.mod"), "utf8");
    const hit = gomod.match(/^\s*module\s+(\S+)/m);
    if (hit) m = hit[1];
  } catch {
    /* no go.mod */
  }
  modCache.set(dir, m);
  return m;
}

// Capture import specs: single `import "x"` and block `import ( "a" \n "b" )`.
function importPaths(src) {
  const out = [];
  const block = /import\s*\(([\s\S]*?)\)/g;
  let m;
  while ((m = block.exec(src)) !== null) {
    for (const s of m[1].matchAll(/"([^"]+)"/g)) out.push(s[1]);
  }
  for (const s of src.matchAll(/^\s*import\s+"([^"]+)"/gm)) out.push(s[1]);
  return out;
}

function resolveImports(dir, file) {
  const src = safeRead(path.join(dir, file));
  if (src == null) return [];
  const mod = modulePath(dir);
  const out = [];
  const seen = new Set();
  for (const imp of importPaths(src)) {
    // only intra-module imports map to in-repo packages
    let sub = null;
    if (mod && imp === mod) sub = "";
    else if (mod && imp.startsWith(mod + "/")) sub = imp.slice(mod.length + 1);
    if (sub == null) continue;
    const pkgDir = path.join(dir, sub);
    let ents;
    try {
      ents = fs.readdirSync(pkgDir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of ents) {
      if (e.isFile() && e.name.endsWith(".go") && !e.name.endsWith("_test.go")) {
        const rel = path.relative(dir, path.join(pkgDir, e.name));
        if (!seen.has(rel)) {
          seen.add(rel);
          out.push(rel);
        }
      }
    }
  }
  return out;
}

// ── func/method definitions & calls (tree-sitter) ─────────────────────────────

function walk(node, visit) {
  visit(node);
  for (const c of node.namedChildren) walk(c, visit);
}

function defName(n) {
  const id = n.childForFieldName("name");
  return id && id.type === "identifier" ? id.text : null;
}

function funcDefs(tree) {
  const out = [];
  walk(tree.rootNode, (n) => {
    if (n.type === "function_declaration" || n.type === "method_declaration") {
      const name = defName(n);
      if (name) out.push({ name, node: n });
    }
  });
  return out;
}

// identifier callees: `f(...)` and the method/func name in `pkg.F(...)` / `x.M(...)`
function calleesOf(node, self) {
  const out = new Set();
  walk(node, (n) => {
    if (n.type !== "call_expression") return;
    const fn = n.childForFieldName("function");
    if (!fn) return;
    let name = null;
    if (fn.type === "identifier") name = fn.text;
    else if (fn.type === "selector_expression") {
      const field = fn.childForFieldName("field");
      if (field && field.type === "field_identifier") name = field.text;
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

// ── repo-wide definition index ────────────────────────────────────────────────

const indexCache = new Map();
function defIndex(dir) {
  if (indexCache.has(dir)) return indexCache.get(dir);
  const idx = new Map();
  const files = walkGo(dir, INDEX_FILE_CAP);
  if (files.length >= INDEX_FILE_CAP) {
    process.stderr.write(`resolve-go: definition index capped at ${INDEX_FILE_CAP} files\n`);
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

function walkGo(dir, cap) {
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
      if (e.name.startsWith(".") || e.name === "vendor" || e.name === "node_modules") continue;
      const p = path.join(d, e.name);
      if (e.isDirectory()) go(p);
      else if (e.name.endsWith(".go") && !e.name.endsWith("_test.go")) out.push(p);
    }
  };
  go(dir);
  return out;
}

function discoverTargets(dir, limit) {
  const out = [];
  for (const abs of walkGo(dir, 400)) {
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

// The top-level funcs, methods, and named types in `file` with 1-based inclusive line ranges —
// AST-accurate symbol boundaries the semantic diff uses to name which symbol an added line lives in.
// A method's name field is a `field_identifier` and a type's is a `type_identifier` (not the plain
// `identifier` that `defName` insists on), so read the "name" field's text directly here. tree-sitter
// positions are 0-based rows, so +1.
function collectSymbols(dir, file) {
  const src = safeRead(path.resolve(dir, file));
  if (src == null) throw new Error(`file not found: ${file}`);
  const tree = parser.parse(src);
  const out = [];
  const push = (n, kind) => {
    const id = n.childForFieldName("name");
    if (id && id.text) out.push({ name: id.text, kind, startLine: n.startPosition.row + 1, endLine: n.endPosition.row + 1 });
  };
  walk(tree.rootNode, (n) => {
    if (n.type === "function_declaration") push(n, "function");
    else if (n.type === "method_declaration") push(n, "method");
    else if (n.type === "type_spec") push(n, "type"); // `type Foo struct/interface/... {…}`
  });
  return out;
}
