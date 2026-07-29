// keel resolver sidecar (TS/JS).
//
// Newline-delimited JSON over stdin/stdout, driven by keel-resolve (Rust). One request
// per line, one response per line, answered in order. Ops:
//   { op:"health" }                          -> { ok, result:{ lang, version, ts } }
//   { op:"imports", dir, file }              -> { ok, result:{ targets:[repo-relative paths] } }
//   { op:"slice", dir, file, symbol, depth } -> { ok, result:{ defs:[{file,symbol,text}] } }
//
// `slice` is the real relevance primitive: it uses the TypeScript **compiler + type
// checker** (not regex) to resolve what a function transitively calls across files —
// through imports, re-exports, aliases, methods, namespaces — and returns the minimal
// cross-file subgraph (target + resolved callees, BFS-bounded). Ported from the proven
// prototype `keel-bench/src/symbol-slice-ts.mjs` (76–98% on real repos). `imports` is a
// cheap relative-import scan kept for quick edges.

import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";

const rl = readline.createInterface({ input: process.stdin });

rl.on("line", async (line) => {
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
      const ts = await ensureTs().catch(() => null);
      respond({ id, ok: true, result: { lang: "ts", version: "0.2", ts: ts ? ts.version : null } });
    } else if (req.op === "imports") {
      respond({ id, ok: true, result: { targets: resolveImports(req.dir, req.file) } });
    } else if (req.op === "slice") {
      const defs = await doSlice(req.dir, req.file, req.symbol, req.depth ?? 2);
      respond({ id, ok: true, result: { defs } });
    } else if (req.op === "targets") {
      respond({ id, ok: true, result: { targets: await discoverTargets(req.dir, req.limit ?? 20) } });
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

// ── relative imports (cheap edges, no compiler) ──────────────────────────────

const IMPORT_RE =
  /import\s+[^'"]*from\s*['"]([^'"]+)['"]|export\s+[^'"]*from\s*['"]([^'"]+)['"]|require\(\s*['"]([^'"]+)['"]\s*\)|import\s*['"]([^'"]+)['"]/g;

function resolveImports(dir, file) {
  const src = fs.readFileSync(path.join(dir, file), "utf8");
  const out = [];
  const seen = new Set();
  let m;
  while ((m = IMPORT_RE.exec(src)) !== null) {
    const spec = m[1] || m[2] || m[3] || m[4];
    if (!spec || !spec.startsWith(".")) continue;
    const resolved = resolveRelative(dir, file, spec);
    if (resolved && !seen.has(resolved)) {
      seen.add(resolved);
      out.push(resolved);
    }
  }
  return out;
}

const EXTS = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];
const INDEX = ["/index.ts", "/index.tsx", "/index.js", "/index.jsx"];
// TS ESM: a .js/.jsx/.mjs/.cjs specifier maps to the .ts/.tsx source on disk.
const JS_TO_TS = { ".js": [".ts", ".tsx"], ".jsx": [".tsx"], ".mjs": [".mts"], ".cjs": [".cts"] };

function resolveRelative(dir, file, spec) {
  const base = path.dirname(path.join(dir, file));
  const cand = path.resolve(base, spec);
  const cands = [];
  const ext = path.extname(cand);
  if (JS_TO_TS[ext]) {
    const stem = cand.slice(0, -ext.length);
    for (const e of JS_TO_TS[ext]) cands.push(stem + e); // config.js → config.ts
  }
  cands.push(cand); // as written
  for (const e of EXTS) cands.push(cand + e); // extensionless
  for (const e of INDEX) cands.push(cand + e); // directory index
  for (const p of cands) {
    try {
      if (fs.statSync(p).isFile()) return path.relative(dir, p);
    } catch {
      /* not this candidate */
    }
  }
  return null;
}

// ── TS-compiler symbol slice (the relevance primitive) ───────────────────────

let TS = null;
async function ensureTs() {
  if (!TS) TS = (await import("typescript")).default ?? (await import("typescript"));
  return TS;
}

// program cache keyed by root dir. (Liveness/incremental refresh is a later increment;
// for now a program is built once per dir and reused.)
const programs = new Map();

function walkFiles(dir) {
  const out = [];
  let ents;
  try {
    ents = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of ents) {
    if (
      e.name.startsWith(".") ||
      e.name === "node_modules" ||
      e.name === "dist" ||
      e.name === "build"
    )
      continue;
    const p = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...walkFiles(p));
    else if (/\.(ts|tsx)$/.test(e.name) && !e.name.endsWith(".d.ts")) out.push(p);
  }
  return out;
}

async function getProgram(dir) {
  if (programs.has(dir)) return programs.get(dir);
  const ts = await ensureTs();
  const fileNames = walkFiles(dir);
  const program = ts.createProgram(fileNames, {
    allowJs: false,
    noEmit: true,
    skipLibCheck: true,
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    noResolve: false,
  });
  const entry = {
    ts,
    program,
    checker: program.getTypeChecker(),
    inRepo: new Set(fileNames),
  };
  programs.set(dir, entry);
  return entry;
}

function helpers(ts, checker, inRepo) {
  const isFuncLike = (n) =>
    ts.isFunctionDeclaration(n) ||
    ts.isMethodDeclaration(n) ||
    ((ts.isVariableDeclaration(n) || ts.isPropertyDeclaration(n)) &&
      n.initializer &&
      (ts.isArrowFunction(n.initializer) || ts.isFunctionExpression(n.initializer)));
  const declName = (n) => {
    try {
      if (n.name && ts.isIdentifier(n.name)) return n.name.text;
    } catch {
      /* no name */
    }
    return null;
  };
  const declText = (n) => n.getText(n.getSourceFile());
  const declFile = (n) => n.getSourceFile().fileName;

  // The type-checker resolution regex can't do: follows the symbol at each identifier,
  // unwraps import/re-export aliases, and keeps only in-repo function-like definitions.
  const calleeDecls = (target) => {
    const out = new Map();
    const visit = (node) => {
      if (ts.isIdentifier(node)) {
        let sym = checker.getSymbolAtLocation(node);
        try {
          if (sym && sym.flags & ts.SymbolFlags.Alias) sym = checker.getAliasedSymbol(sym);
        } catch {
          /* not an alias */
        }
        const decls = sym?.getDeclarations?.() || [];
        for (const d of decls) {
          if (isFuncLike(d) && inRepo.has(declFile(d))) {
            const k = declFile(d) + "#" + (declName(d) || node.text);
            if (!out.has(k)) out.set(k, d);
          }
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(target);
    return [...out.values()];
  };
  return { isFuncLike, declName, declText, declFile, calleeDecls };
}

async function doSlice(dir, file, symbol, depth, max = 40) {
  const { ts, program, checker, inRepo } = await getProgram(dir);
  const { isFuncLike, declName, declText, declFile, calleeDecls } = helpers(ts, checker, inRepo);

  const abs = path.resolve(dir, file);
  const sf =
    program.getSourceFile(abs) ||
    program.getSourceFiles().find((s) => s.fileName === abs || s.fileName.endsWith("/" + file));
  if (!sf) throw new Error(`file not in program: ${file}`);

  let target = null;
  const find = (n) => {
    if (!target && isFuncLike(n) && declName(n) === symbol) target = n;
    if (!target) ts.forEachChild(n, find);
  };
  find(sf);
  if (!target) throw new Error(`symbol not found: ${symbol} in ${file}`);

  const chosen = new Map();
  const key = (n) => declFile(n) + "#" + declName(n);
  chosen.set(key(target), target);
  let frontier = [target];
  for (let d = 0; d < depth && chosen.size < max; d++) {
    const next = [];
    for (const s of frontier)
      for (const c of calleeDecls(s)) {
        const k = key(c);
        if (!chosen.has(k)) {
          chosen.set(k, c);
          next.push(c);
        }
      }
    frontier = next;
    if (!next.length) break;
  }
  return [...chosen.values()].map((n) => ({
    file: path.relative(dir, declFile(n)),
    symbol: declName(n),
    text: declText(n),
  }));
}

// Discover substantial functions that have at least one cross-file callee — useful
// targets for benchmarking the slicer on a real repo.
async function discoverTargets(dir, limit) {
  const { ts, program, checker, inRepo } = await getProgram(dir);
  const { isFuncLike, declName, declText, declFile, calleeDecls } = helpers(ts, checker, inRepo);
  const out = [];
  for (const f of inRepo) {
    if (out.length >= limit) break;
    const sf = program.getSourceFile(f);
    if (!sf) continue;
    const visit = (n) => {
      if (out.length >= limit) return;
      if (isFuncLike(n) && declName(n) && declText(n).length > 180) {
        if (calleeDecls(n).some((c) => declFile(c) !== f)) {
          out.push({ file: path.relative(dir, f), symbol: declName(n) });
        }
      }
      ts.forEachChild(n, visit);
    };
    visit(sf);
  }
  return out;
}
