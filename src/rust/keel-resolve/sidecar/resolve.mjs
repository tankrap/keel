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
      respond({ id, ok: true, result: { lang: "ts", version: "0.3", ts: ts ? ts.version : null } });
    } else if (req.op === "imports") {
      respond({ id, ok: true, result: { targets: await resolveImports(req.dir, req.file) } });
    } else if (req.op === "slice") {
      const defs = await doSlice(req.dir, req.file, req.symbol, req.depth ?? 2);
      respond({ id, ok: true, result: { defs } });
    } else if (req.op === "targets") {
      respond({ id, ok: true, result: { targets: await discoverTargets(req.dir, req.limit ?? 20) } });
    } else if (req.op === "symbols") {
      respond({ id, ok: true, result: { symbols: await collectSymbols(req.dir, req.file) } });
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

// ── relative imports (quick edges) ───────────────────────────────────────────
//
// Specifiers come from the TypeScript scanner (`ts.preProcessFile`), so import-like text inside
// comments or string literals does NOT produce a false edge — the regex below can't tell those
// apart and over-matched (bad rdeps → wrong blast radius in a brief). The regex is kept only as a
// fallback for when the compiler can't be loaded.

const IMPORT_RE =
  /import\s+[^'"]*from\s*['"]([^'"]+)['"]|export\s+[^'"]*from\s*['"]([^'"]+)['"]|require\(\s*['"]([^'"]+)['"]\s*\)|import\s*['"]([^'"]+)['"]/g;

/// Module specifiers imported/required/re-exported by `src`. Uses the TS preprocessor (comment- and
/// string-aware); on any compiler-load failure, falls back to the cheap regex (may over-match).
async function importSpecifiers(src) {
  try {
    const ts = await ensureTs();
    return ts.preProcessFile(src, /*readImportFiles*/ true, /*detectJavaScriptImports*/ true)
      .importedFiles.map((f) => f.fileName);
  } catch (e) {
    // Degrade to the over-matching regex, but log it (the daemon captures sidecar stderr) so a
    // persistent compiler-load failure — which would silently reintroduce false edges — is visible.
    console.error(`keel-resolve: import scan fell back to regex (${(e && e.message) || e})`);
    const out = [];
    IMPORT_RE.lastIndex = 0;
    let m;
    while ((m = IMPORT_RE.exec(src)) !== null) out.push(m[1] || m[2] || m[3] || m[4]);
    return out.filter(Boolean);
  }
}

async function resolveImports(dir, file) {
  const src = fs.readFileSync(path.join(dir, file), "utf8");
  const out = [];
  const seen = new Set();
  for (const spec of await importSpecifiers(src)) {
    if (!spec || !spec.startsWith(".")) continue; // relative only; bare/external specifiers omitted
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

// One TypeScript LanguageService per root dir. A one-shot `createProgram` froze the file
// set at first slice, so files added/changed afterward were invisible — the graph went live
// via fs-watch but slices stayed stale. The LanguageService is incremental: on each slice we
// re-stat the tree, and it re-parses only files whose (mtime,size) changed, reusing the rest
// through the document registry — so the brief's *context* is live too, and cheap after the
// first (expensive) build.
const services = new Map();

function tsCompilerOptions(ts, dir) {
  const opts = {
    // Include JavaScript so `symbols`/`slice` work on .js/.jsx/.mjs/.cjs too, not just TypeScript.
    // (node_modules/dist/build are excluded by walkFiles, so this doesn't pull in dependencies.)
    allowJs: true,
    checkJs: false,
    noEmit: true,
    skipLibCheck: true,
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    noResolve: false,
  };
  // Honor the project's tsconfig `baseUrl` + `paths` so monorepo path aliases (`@app/*`,
  // `vs/*`, …) resolve cross-package — the ~30-35% of monorepo deps a naive resolver misses.
  try {
    const cfg = ts.readConfigFile(path.join(dir, "tsconfig.json"), ts.sys.readFile);
    const co = (cfg && cfg.config && cfg.config.compilerOptions) || {};
    if (co.paths) {
      opts.paths = co.paths;
      opts.baseUrl = path.resolve(dir, co.baseUrl || ".");
    } else if (co.baseUrl) {
      opts.baseUrl = path.resolve(dir, co.baseUrl);
    }
  } catch {
    /* no/invalid tsconfig — fall back to the defaults above */
  }
  return opts;
}

// A LanguageServiceHost backed by the live working tree. `refresh()` re-walks + re-stats so
// the service sees the current files and versions; the service diffs versions to decide what
// to re-parse.
function makeTsHost(ts, dir) {
  let fileNames = [];
  const versions = new Map();
  // Re-walk + re-stat; returns whether anything changed (new / removed / mtime+size differs),
  // so getProgram can skip the (expensive) program rebuild when the tree is untouched.
  const refresh = () => {
    let changed = false;
    fileNames = walkFiles(dir);
    const now = new Set(fileNames);
    for (const f of fileNames) {
      let v = "0";
      try {
        const s = fs.statSync(f);
        v = `${s.mtimeMs}:${s.size}`;
      } catch {
        /* unreadable — leave version */
      }
      if (versions.get(f) !== v) {
        versions.set(f, v);
        changed = true;
      }
    }
    for (const f of [...versions.keys()]) {
      if (!now.has(f)) {
        versions.delete(f);
        changed = true;
      }
    }
    return changed;
  };
  const host = {
    getScriptFileNames: () => fileNames,
    getScriptVersion: (f) => versions.get(f) ?? "0",
    getScriptSnapshot: (f) => {
      try {
        return ts.ScriptSnapshot.fromString(fs.readFileSync(f, "utf8"));
      } catch {
        return undefined;
      }
    },
    getCurrentDirectory: () => dir,
    getCompilationSettings: () => tsCompilerOptions(ts, dir),
    getDefaultLibFileName: (o) => ts.getDefaultLibFilePath(o),
    fileExists: ts.sys.fileExists,
    readFile: ts.sys.readFile,
    readDirectory: ts.sys.readDirectory,
    directoryExists: ts.sys.directoryExists,
    getDirectories: ts.sys.getDirectories,
    realpath: ts.sys.realpath,
  };
  return { host, refresh, fileNames: () => fileNames };
}

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
    else if (/\.(ts|tsx|mts|cts|js|jsx|mjs|cjs)$/.test(e.name) && !e.name.endsWith(".d.ts")) out.push(p);
  }
  return out;
}

async function getProgram(dir) {
  const ts = await ensureTs();
  let entry = services.get(dir);
  if (!entry) {
    const h = makeTsHost(ts, dir);
    const service = ts.createLanguageService(h.host, ts.createDocumentRegistry());
    entry = { h, service, cached: null };
    services.set(dir, entry);
  }
  const changed = entry.h.refresh(); // re-stat so the program reflects the CURRENT working tree
  // Reuse the assembled program + checker when nothing changed — building them is the ~90 ms
  // cost per slice; the re-stat above is what keeps it correct (live) between slices.
  if (!changed && entry.cached) return entry.cached;
  const program = entry.service.getProgram();
  if (!program) throw new Error("no TypeScript program (no source files?)");
  entry.cached = { ts, program, checker: program.getTypeChecker(), inRepo: new Set(entry.h.fileNames()) };
  return entry.cached;
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

// The definitions in `file` with their 1-based line ranges — the AST-accurate symbol boundaries the
// semantic diff uses to say which function an added line lives in. Nested defs are included, so a
// caller can pick the innermost (smallest) range that contains a line. Function-like, class,
// interface, enum, and method declarations with a name.
async function collectSymbols(dir, file) {
  const { ts, program } = await getProgram(dir);
  const abs = path.resolve(dir, file);
  const sf =
    program.getSourceFile(abs) ||
    program.getSourceFiles().find((s) => s.fileName === abs || s.fileName.endsWith("/" + file));
  if (!sf) throw new Error(`file not in program: ${file}`);

  const out = [];
  const nameOf = (n) => (n.name && ts.isIdentifier(n.name) ? n.name.text : null);
  // An anonymous `export default function/class` still needs a name for attribution — "default".
  const isDefault = (n) => (n.modifiers || []).some((m) => m.kind === ts.SyntaxKind.DefaultKeyword);
  const defName = (n) => nameOf(n) || (isDefault(n) ? "default" : null);
  const push = (n, kind, name) => {
    if (!name) return;
    const start = sf.getLineAndCharacterOfPosition(n.getStart(sf)).line + 1;
    const end = sf.getLineAndCharacterOfPosition(n.getEnd()).line + 1;
    out.push({ name, kind, startLine: start, endLine: end });
  };
  const visit = (n) => {
    if (ts.isFunctionDeclaration(n)) push(n, "function", defName(n));
    else if (ts.isClassDeclaration(n)) push(n, "class", defName(n));
    else if (ts.isMethodDeclaration(n)) push(n, "method", nameOf(n));
    else if (ts.isConstructorDeclaration(n)) push(n, "constructor", "constructor");
    else if (ts.isGetAccessorDeclaration(n)) push(n, "getter", nameOf(n));
    else if (ts.isSetAccessorDeclaration(n)) push(n, "setter", nameOf(n));
    else if (ts.isInterfaceDeclaration(n)) push(n, "interface", nameOf(n));
    else if (ts.isEnumDeclaration(n)) push(n, "enum", nameOf(n));
    else if (ts.isModuleDeclaration(n)) push(n, "namespace", nameOf(n));
    else if ((ts.isVariableDeclaration(n) || ts.isPropertyDeclaration(n)) && n.initializer) {
      const init = n.initializer;
      if (ts.isArrowFunction(init) || ts.isFunctionExpression(init))
        push(n, "function", nameOf(n)); // `const f = () => {…}` / `const f = function () {…}`
      else if (ts.isClassExpression(init)) push(n, "class", nameOf(n)); // `const C = class {…}`
    }
    ts.forEachChild(n, visit);
  };
  visit(sf);
  return out;
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
