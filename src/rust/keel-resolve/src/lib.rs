//! Language-resolver sidecar client.
//!
//! The Rust core owns the store, the graph, and coordination; **symbol resolution**
//! is delegated to per-language sidecar subprocesses, because the best resolver for
//! each language lives in its own ecosystem (TypeScript compiler + pyright for the
//! TS/Python spine, gopls for Go, tree-sitter for breadth) and shouldn't be rewritten
//! in Rust (Linear NEW-1101 / NEW-1067).
//!
//! Protocol: newline-delimited JSON over the child's stdin/stdout. Each request is
//! `{ "id": n, "op": "...", ... }`; each response is `{ "id": n, "ok": bool, ... }`.
//! Requests are answered in order (one in flight at a time), so `id` is a correlation
//! check, not a multiplexer — that's enough for the single-daemon model.
//!
//! This first cut resolves **relative imports** (real, useful edges) to prove the seam;
//! a later increment swaps in the proven TS-compiler symbol slicer behind the same
//! `imports`/`resolve` ops.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Safety net against a hung/wedged sidecar. A resolver that stops answering must not
/// hang the caller forever (that would wedge every `brief` through the daemon). Generous
/// so a legitimately slow first `slice` on a large monorepo (TS program creation) isn't
/// falsely killed, but bounded so "forever" becomes "error + recovery".
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// One definition in a symbol slice: a function-like declaration with its repo-relative
/// `file`, `symbol` name, and source `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceDef {
    pub file: String,
    pub symbol: String,
    pub text: String,
}

/// A definition in a file and its 1-based, inclusive line range — the AST-accurate symbol boundary
/// the semantic diff uses to name which function/class an added line lives in. `kind` is one of
/// `function` / `method` / `class` / `interface` / `enum`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRange {
    pub name: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Why a sidecar stopped being usable. A **crash** (the process exited / the pipe broke /
/// the id stream desynced) is healed by respawning a fresh child on the next call. A
/// **wedge** (a request timed out — the process is alive but stuck) is *not* retried: a
/// fresh process would likely wedge on the same input, and retrying would just burn another
/// timeout, so wedged calls fail fast until the caller/operator intervenes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeadKind {
    Crashed,
    Wedged,
}

pub struct Sidecar {
    script: PathBuf,
    child: Child,
    stdin: ChildStdin,
    /// Lines pumped off the child's stdout by a background reader thread, so `call` can
    /// wait on them with a timeout — a plain `read_line` on a pipe can't be timed out.
    rx: mpsc::Receiver<io::Result<String>>,
    /// The sidecar's most recent stderr line, captured by a drain thread. Folded into a crash
    /// error so "sidecar closed" becomes "sidecar closed: <the node error it printed>".
    last_stderr: Arc<Mutex<Option<String>>>,
    next_id: u64,
    timeout: Duration,
    /// `None` while healthy. Set when the sidecar becomes unusable; a crash is healed on the
    /// next call (respawn), a wedge fails fast (see [`DeadKind`]).
    dead: Option<DeadKind>,
}

impl Sidecar {
    /// Spawn `node <script>` as a resolver sidecar. Errors if `node` isn't on PATH.
    pub fn spawn(script: &Path) -> io::Result<Sidecar> {
        let mut child = Command::new("node")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin was piped");
        // Drain the sidecar's stderr rather than inheriting it (which would silently interleave
        // node output into the daemon's own stderr). Each line is forwarded, tagged, to our stderr
        // so it's still visible with context, and the latest is kept for a crash error's `hint`.
        let last_stderr = Arc::new(Mutex::new(None::<String>));
        if let Some(stderr) = child.stderr.take() {
            let slot = Arc::clone(&last_stderr);
            let tag = script.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stderr);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    // Read raw bytes, not `read_line`: a non-UTF-8 stderr line would make `read_line`
                    // return an error, and stopping the drain there could let the child fill the pipe
                    // buffer and block on `write` → stall its stdout → a wrongful wedge. `read_until`
                    // + lossy decode never stops on bad bytes; only a true EOF/IO error ends it.
                    match r.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break, // EOF or a real pipe error — child's stderr is done
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&buf);
                            let trimmed = line.trim_end();
                            if !trimmed.is_empty() {
                                eprintln!("[keel-resolve sidecar {tag}] {trimmed}");
                                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(trimmed.to_string());
                            }
                        }
                    }
                }
            });
        }
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        // Pump stdout on a dedicated thread. A blocking `read_line` on a child pipe has
        // no timeout, so the thread converts each line into a channel send that `call`
        // can `recv_timeout` on. The thread exits on EOF, read error, or when the
        // receiver is dropped (Sidecar dropped) — no leak.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) => break, // child closed stdout
                Ok(_) => {
                    if tx.send(Ok(line)).is_err() {
                        break; // Sidecar dropped
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        });
        Ok(Sidecar {
            script: script.to_path_buf(),
            child,
            stdin,
            rx,
            last_stderr,
            next_id: 0,
            timeout: DEFAULT_TIMEOUT,
            dead: None,
        })
    }

    /// `: <last stderr line>` if the sidecar printed one, else empty — appended to a crash error so
    /// the failure carries the node-side reason instead of an opaque "sidecar closed".
    fn stderr_hint(&self) -> String {
        match self.last_stderr.lock().unwrap_or_else(|p| p.into_inner()).as_deref() {
            Some(s) if !s.is_empty() => format!(": {s}"),
            _ => String::new(),
        }
    }

    /// Override the per-request timeout (default 120s). The daemon can tune this to its
    /// own SLO; a shorter bound trades false kills of a slow first slice for faster
    /// recovery from a wedged sidecar.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Kill the child and record why it's unusable, so the next call heals (crash) or fails
    /// fast (wedge) instead of blocking on or desyncing against a subprocess we can't trust.
    fn kill_dead(&mut self, kind: DeadKind) {
        self.dead = Some(kind);
        let _ = self.child.kill();
    }

    /// Replace a crashed child with a fresh one, preserving the configured timeout. The old
    /// child is killed by `Drop` when the previous `self` is overwritten.
    fn respawn(&mut self) -> io::Result<()> {
        let timeout = self.timeout;
        let fresh = Sidecar::spawn(&self.script.clone())?;
        *self = fresh;
        self.timeout = timeout;
        Ok(())
    }

    fn call(&mut self, mut req: Value) -> io::Result<Value> {
        match self.dead {
            None => {}
            Some(DeadKind::Crashed) => self.respawn()?, // self-heal a crashed sidecar
            Some(DeadKind::Wedged) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "resolver sidecar wedged (timed out); not retrying",
                ));
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        let line =
            serde_json::to_string(&req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // A write failure means the pipe is gone — the process died (crash). Bail, heal next.
        if let Err(e) = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
        {
            self.kill_dead(DeadKind::Crashed);
            return Err(e);
        }

        let resp = match self.rx.recv_timeout(self.timeout) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                self.kill_dead(DeadKind::Crashed); // reader thread saw an IO error
                return Err(e);
            }
            Err(RecvTimeoutError::Timeout) => {
                self.kill_dead(DeadKind::Wedged); // alive but stuck — don't retry
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("resolver sidecar did not respond within {:?}", self.timeout),
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.kill_dead(DeadKind::Crashed); // reader thread ended = child closed stdout
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("resolver sidecar closed{}", self.stderr_hint()),
                ));
            }
        };
        let v: Value = serde_json::from_str(resp.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if v.get("id").and_then(Value::as_u64) != Some(id) {
            // The reply stream is out of sync — respawn to resync on the next call.
            self.kill_dead(DeadKind::Crashed);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "sidecar response id mismatch"));
        }
        Ok(v)
    }

    /// Health/handshake — returns the sidecar's `{ lang, version, ... }`.
    pub fn health(&mut self) -> io::Result<Value> {
        let r = self.call(json!({ "op": "health" }))?;
        Ok(r.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Resolve the relative-import targets of `file` (repo-relative paths). External
    /// / bare specifiers (e.g. `node:fs`, `react`) are omitted — those are edges to
    /// packages, not in-repo files.
    pub fn imports(&mut self, dir: &Path, file: &str) -> io::Result<Vec<String>> {
        let r = self.call(json!({ "op": "imports", "dir": dir.to_string_lossy(), "file": file }))?;
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(r["result"]["targets"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default())
        } else {
            let msg = r.get("error").and_then(Value::as_str).unwrap_or("sidecar error");
            Err(io::Error::other(msg.to_string()))
        }
    }

    /// TS-compiler symbol slice: the minimal cross-file subgraph for `symbol` in `file`
    /// — the target plus the function-like definitions it transitively calls, resolved
    /// through imports / re-exports / aliases by the type checker. This is the relevance
    /// primitive (76–98% on real repos in the prototype).
    pub fn slice(
        &mut self,
        dir: &Path,
        file: &str,
        symbol: &str,
        depth: u32,
    ) -> io::Result<Vec<SliceDef>> {
        let r = self.call(json!({
            "op": "slice", "dir": dir.to_string_lossy(), "file": file, "symbol": symbol, "depth": depth
        }))?;
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            let defs = r["result"]["defs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|d| {
                            Some(SliceDef {
                                file: d.get("file")?.as_str()?.to_string(),
                                symbol: d
                                    .get("symbol")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                text: d.get("text")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(defs)
        } else {
            let msg = r.get("error").and_then(Value::as_str).unwrap_or("slice error");
            Err(io::Error::other(msg.to_string()))
        }
    }

    /// The definitions in `file` with their 1-based inclusive line ranges (AST-accurate, via the TS
    /// compiler). Nested defs are included, so the caller can pick the innermost range containing a
    /// line — the intended lookup. Names are NOT unique (overload signatures repeat a name, each with
    /// its own range), so resolve by range, not by a name-keyed map. Errors if the sidecar has no
    /// TypeScript or the file isn't in its program — callers that want a graceful fallback should
    /// treat an `Err` as "no symbol info for this file".
    pub fn symbols(&mut self, dir: &Path, file: &str) -> io::Result<Vec<SymbolRange>> {
        let r = self.call(json!({ "op": "symbols", "dir": dir.to_string_lossy(), "file": file }))?;
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            let syms = r["result"]["symbols"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| {
                            Some(SymbolRange {
                                name: s.get("name")?.as_str()?.to_string(),
                                kind: s.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
                                start_line: s.get("startLine")?.as_u64()? as u32,
                                end_line: s.get("endLine")?.as_u64()? as u32,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(syms)
        } else {
            let msg = r.get("error").and_then(Value::as_str).unwrap_or("symbols error");
            Err(io::Error::other(msg.to_string()))
        }
    }

    /// Discover substantial functions with a cross-file callee — `(file, symbol)`
    /// pairs, a bench/eval helper for exercising the slicer on a real repo.
    pub fn targets(&mut self, dir: &Path, limit: usize) -> io::Result<Vec<(String, String)>> {
        let r =
            self.call(json!({ "op": "targets", "dir": dir.to_string_lossy(), "limit": limit }))?;
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(r["result"]["targets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| {
                            Some((
                                t.get("file")?.as_str()?.to_string(),
                                t.get("symbol")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default())
        } else {
            let msg = r.get("error").and_then(Value::as_str).unwrap_or("targets error");
            Err(io::Error::other(msg.to_string()))
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The resolver interface the graph and brief depend on, so they can be driven by either a
/// single [`Sidecar`] or a multi-language [`Router`] without caring which.
pub trait Resolve {
    fn imports(&mut self, dir: &Path, file: &str) -> io::Result<Vec<String>>;
    fn slice(&mut self, dir: &Path, file: &str, symbol: &str, depth: u32) -> io::Result<Vec<SliceDef>>;
}

impl Resolve for Sidecar {
    fn imports(&mut self, dir: &Path, file: &str) -> io::Result<Vec<String>> {
        Sidecar::imports(self, dir, file) // inherent method (explicit to avoid trait recursion)
    }
    fn slice(&mut self, dir: &Path, file: &str, symbol: &str, depth: u32) -> io::Result<Vec<SliceDef>> {
        Sidecar::slice(self, dir, file, symbol, depth)
    }
}

/// The language keel routes a path to, by extension.
fn lang_of(file: &str) -> Option<&'static str> {
    match file.rsplit('.').next().unwrap_or("") {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Some("ts"),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Some("c"),
        "py" | "pyi" => Some("py"),
        "go" => Some("go"),
        _ => None,
    }
}

/// Routes each file to the right language sidecar (TS vs C), spawning them lazily and
/// reusing them — so one process can serve a mixed-language repo, and a repo that never
/// touches a language never pays to start its sidecar. Files of an unknown language yield
/// no edges / no slice (rather than an error), so the graph simply omits them.
pub struct Router {
    dir: PathBuf,
    sidecars: HashMap<&'static str, Sidecar>,
}

impl Router {
    /// A router whose sidecar scripts live in `sidecar_dir` (expects `resolve.mjs` and
    /// `resolve-c.mjs`). Sidecars are spawned on first use.
    pub fn new(sidecar_dir: &Path) -> Router {
        Router { dir: sidecar_dir.to_path_buf(), sidecars: HashMap::new() }
    }

    fn script(&self, lang: &str) -> PathBuf {
        self.dir.join(match lang {
            "c" => "resolve-c.mjs",
            "py" => "resolve-py.mjs",
            "go" => "resolve-go.mjs",
            _ => "resolve.mjs",
        })
    }

    fn sidecar(&mut self, lang: &'static str) -> io::Result<&mut Sidecar> {
        if !self.sidecars.contains_key(lang) {
            let sc = Sidecar::spawn(&self.script(lang))?;
            self.sidecars.insert(lang, sc);
        }
        Ok(self.sidecars.get_mut(lang).expect("just inserted"))
    }

    /// Languages with a sidecar currently running (for diagnostics/tests).
    pub fn active_langs(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.sidecars.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

impl Resolve for Router {
    fn imports(&mut self, dir: &Path, file: &str) -> io::Result<Vec<String>> {
        match lang_of(file) {
            Some(lang) => self.sidecar(lang)?.imports(dir, file),
            None => Ok(Vec::new()),
        }
    }
    fn slice(&mut self, dir: &Path, file: &str, symbol: &str, depth: u32) -> io::Result<Vec<SliceDef>> {
        match lang_of(file) {
            Some(lang) => self.sidecar(lang)?.slice(dir, file, symbol, depth),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("keel-resolve-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar/resolve.mjs")
    }

    #[test]
    fn health_and_relative_imports() {
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping resolver test: node not available ({e})");
                return;
            }
        };
        let h = sc.health().unwrap();
        assert_eq!(h["lang"], "ts");

        let dir = tmp();
        fs::write(dir.join("a.ts"), "import { x } from './b';\nimport fs from 'node:fs';\n").unwrap();
        fs::write(dir.join("b.ts"), "export const x = 1;\n").unwrap();
        fs::create_dir_all(dir.join("util")).unwrap();
        fs::write(dir.join("util/index.ts"), "export const u = 2;\n").unwrap();
        fs::write(dir.join("c.ts"), "import { u } from './util';\n").unwrap();

        let targets = sc.imports(&dir, "a.ts").unwrap();
        assert!(targets.iter().any(|t| t == "b.ts"), "resolved ./b → b.ts; got {targets:?}");
        assert!(!targets.iter().any(|t| t.contains("node:")), "external specifiers omitted");

        // directory import resolves to its index file
        let ct = sc.imports(&dir, "c.ts").unwrap();
        assert!(ct.iter().any(|t| t == "util/index.ts"), "resolved ./util → util/index.ts; got {ct:?}");

        // TS ESM convention: a `.js` specifier resolves to the `.ts` source on disk
        fs::write(dir.join("mod.ts"), "export const m = 3;\n").unwrap();
        fs::write(dir.join("main.ts"), "import { m } from './mod.js';\n").unwrap();
        let mt = sc.imports(&dir, "main.ts").unwrap();
        assert!(mt.iter().any(|t| t == "mod.ts"), "'./mod.js' → mod.ts (TS ESM); got {mt:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_ignore_comments_and_strings() {
        // The TS scanner (preProcessFile) must not treat import-like text in comments or string
        // literals as edges — the old regex did, producing false deps/rdeps. `ghost.ts` exists on
        // disk, so the ONLY reason it's excluded is comment/string awareness, not a resolve miss.
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping resolver test: node not available ({e})");
                return;
            }
        };
        // The fix is comment/string-awareness from the TS scanner; without TS installed the sidecar
        // takes the regex fallback (which over-matches by design), so skip rather than fail there.
        if sc.health().unwrap().get("ts").map(|v| v.is_null()).unwrap_or(true) {
            eprintln!("skipping import-edges test: typescript not installed in sidecar");
            return;
        }
        let dir = tmp();
        fs::write(dir.join("real.ts"), "export const r = 1;\n").unwrap();
        fs::write(dir.join("ghost.ts"), "export const g = 2;\n").unwrap();
        let src = "import { r } from \"./real\";\n\
                   // import { g } from \"./ghost\";\n\
                   /* import { g } from \"./ghost\"; */\n\
                   const s = \"import x from './ghost'\";\n\
                   const t = `require(\"./ghost\")`;\n";
        fs::write(dir.join("a.ts"), src).unwrap();

        let targets = sc.imports(&dir, "a.ts").unwrap();
        assert!(targets.iter().any(|t| t == "real.ts"), "real import resolved; got {targets:?}");
        assert!(
            !targets.iter().any(|t| t == "ghost.ts"),
            "no false edge from a commented-out or stringified import; got {targets:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn slice_resolves_cross_file_through_reexport() {
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping slice test: node not available ({e})");
                return;
            }
        };
        let h = sc.health().unwrap();
        if h.get("ts").map(|v| v.is_null()).unwrap_or(true) {
            eprintln!("skipping slice test: typescript not installed in sidecar");
            return;
        }

        let dir = tmp();
        // helperB is defined in b.ts, re-exported through reexport.ts, and called by doA
        // in a.ts — so resolving it requires following the re-export alias (regex can't).
        fs::write(dir.join("b.ts"), "export function helperB(x: number): number {\n  return x * 2;\n}\n").unwrap();
        fs::write(dir.join("reexport.ts"), "export { helperB } from './b';\n").unwrap();
        fs::write(
            dir.join("a.ts"),
            "import { helperB } from './reexport';\nexport function doA(): number {\n  return helperB(21) + helperB(0);\n}\n",
        )
        .unwrap();

        let defs = sc.slice(&dir, "a.ts", "doA", 2).unwrap();
        assert!(defs.iter().any(|d| d.symbol == "doA" && d.file == "a.ts"), "target in slice; got {defs:?}");
        let helper = defs.iter().find(|d| d.symbol == "helperB");
        assert!(helper.is_some(), "helperB pulled into the slice; got {defs:?}");
        let helper = helper.unwrap();
        assert_eq!(helper.file, "b.ts", "resolved THROUGH the re-export to the real def in b.ts");
        assert!(helper.text.contains("x * 2"), "slice includes helperB's body");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The `Router` picks the sidecar per file extension and spawns each lazily: a TS file
    /// routes to the TS sidecar, a C file to the C sidecar, and an unknown extension yields
    /// no edges (not an error). One router serves a mixed-language repo.
    #[test]
    fn router_dispatches_by_extension_and_spawns_lazily() {
        let sidecar_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar");
        if which::exists_node().is_none() {
            eprintln!("skipping router test: node not available");
            return;
        }
        let mut r = Router::new(&sidecar_dir);
        assert!(r.active_langs().is_empty(), "no sidecar spawned until first use");

        let dir = tmp();
        fs::write(dir.join("a.ts"), "import { b } from './b';\n").unwrap();
        fs::write(dir.join("b.ts"), "export const b = 1;\n").unwrap();
        fs::create_dir_all(dir.join("include")).unwrap();
        fs::write(dir.join("include/u.h"), "int helper(int);\n").unwrap();
        fs::write(dir.join("m.c"), "#include <u.h>\nint f(void){return 0;}\n").unwrap();

        // TS file → TS sidecar resolves the relative import
        let ts = r.imports(&dir, "a.ts").unwrap();
        assert!(ts.iter().any(|t| t == "b.ts"), "TS routed; got {ts:?}");
        // C file → C sidecar resolves the include
        let c = r.imports(&dir, "m.c").unwrap();
        assert!(c.iter().any(|t| t == "include/u.h"), "C routed; got {c:?}");
        // unknown extension → no edges, no error, no sidecar
        assert!(r.imports(&dir, "readme.md").unwrap().is_empty());

        assert_eq!(r.active_langs(), vec!["c", "ts"], "both sidecars spawned, exactly once each");
        let _ = fs::remove_dir_all(&dir);
    }

    // tiny node-availability probe so the router test can skip cleanly
    mod which {
        pub fn exists_node() -> Option<()> {
            std::process::Command::new("node")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()
                .filter(|s| s.success())
                .map(|_| ())
        }
    }

    /// The TS slicer must be LIVE: a file created after the program was first built still
    /// slices (the LanguageService re-parses the changed tree). Regression guard for the
    /// stale-`ts.Program` bug fs-watch surfaced.
    #[test]
    fn ts_slice_is_live_to_files_added_after_first_build() {
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping TS-liveness test: node not available ({e})");
                return;
            }
        };
        if sc.health().unwrap().get("ts").map(|v| v.is_null()).unwrap_or(true) {
            eprintln!("skipping TS-liveness test: typescript not installed");
            return;
        }
        let dir = tmp();
        fs::write(dir.join("util.ts"), "export function helper(x: number): number { return x * 2; }\n").unwrap();
        fs::write(dir.join("a.ts"), "import { helper } from './util.js';\nexport function doA(): number { return helper(1); }\n").unwrap();
        // first slice builds the LanguageService program over {util, a}
        let d1 = sc.slice(&dir, "a.ts", "doA", 2).unwrap();
        assert!(d1.iter().any(|d| d.symbol == "helper"), "baseline slice resolves; got {d1:?}");

        // add a NEW file AFTER the program was built — the LS must pick it up
        fs::write(dir.join("b.ts"), "import { helper } from './util.js';\nexport function doB(): number { return helper(2); }\n").unwrap();
        let d2 = sc.slice(&dir, "b.ts", "doB", 2).unwrap();
        assert!(d2.iter().any(|d| d.symbol == "doB" && d.file == "b.ts"), "new file's symbol sliced; got {d2:?}");
        assert!(d2.iter().any(|d| d.symbol == "helper" && d.file == "util.ts"), "cross-file live in the new file; got {d2:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Monorepo path aliases from `tsconfig.json` (`@util/*` → `src/util/*`) must resolve in
    /// the slice — the cross-package deps a naive relative-only resolver misses (NEW-1080).
    #[test]
    fn slice_resolves_tsconfig_path_alias() {
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping tsconfig-paths test: node not available ({e})");
                return;
            }
        };
        if sc.health().unwrap().get("ts").map(|v| v.is_null()).unwrap_or(true) {
            eprintln!("skipping tsconfig-paths test: typescript not installed");
            return;
        }
        let dir = tmp();
        fs::write(dir.join("tsconfig.json"), r#"{"compilerOptions":{"baseUrl":".","paths":{"@util/*":["src/util/*"]}}}"#).unwrap();
        fs::create_dir_all(dir.join("src/util")).unwrap();
        fs::create_dir_all(dir.join("src/app")).unwrap();
        fs::write(dir.join("src/util/shared.ts"), "export function shared(x: number): number { return x * 2; }\n").unwrap();
        fs::write(dir.join("src/app/main.ts"), "import { shared } from \"@util/shared\";\nexport function run(): number { return shared(21); }\n").unwrap();

        let defs = sc.slice(&dir, "src/app/main.ts", "run", 2).unwrap();
        assert!(
            defs.iter().any(|d| d.symbol == "shared" && d.file == "src/util/shared.ts"),
            "path-aliased import resolved cross-package; got {defs:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The `symbols` op reports each definition's name, kind, and 1-based inclusive line range — the
    /// AST-accurate boundaries the semantic diff uses to name which function an added line lives in.
    /// Nested defs (an arrow fn inside a fn, a method inside a class) are included.
    #[test]
    fn ts_symbols_reports_definitions_with_line_ranges() {
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping symbols test: node not available ({e})");
                return;
            }
        };
        if sc.health().unwrap().get("ts").map(|v| v.is_null()).unwrap_or(true) {
            eprintln!("skipping symbols test: typescript not installed in sidecar");
            return;
        }
        let dir = tmp();
        // 1: fn outer {  2: const helper = (y) => {  3: body  4: };  5: return  6: }  7: class Order {  8: method  9: }
        let src = "export function outer(x: number) {\n  const helper = (y: number) => {\n    return y * 2;\n  };\n  return helper(x);\n}\nexport class Order {\n  total(): number { return 0; }\n}\n";
        fs::write(dir.join("m.ts"), src).unwrap();

        let syms = sc.symbols(&dir, "m.ts").unwrap();
        let by = |name: &str| syms.iter().find(|s| s.name == name).cloned();

        let outer = by("outer").expect("outer fn found");
        assert_eq!(outer.kind, "function");
        assert_eq!((outer.start_line, outer.end_line), (1, 6), "outer spans its whole body");

        let helper = by("helper").expect("nested arrow fn found");
        assert_eq!(helper.kind, "function");
        assert_eq!(helper.start_line, 2, "helper starts at its declaration");
        assert!(helper.end_line >= 3 && helper.end_line <= 4, "helper is the INNER range; got {helper:?}");

        assert_eq!(by("Order").expect("class found").kind, "class");
        assert_eq!(by("total").expect("method found").kind, "method");

        // broadened coverage (review): anonymous default, class expression, constructor
        let src2 = "export default function () {\n  return 1;\n}\nconst Widget = class {\n  constructor() {}\n  render() {}\n};\n";
        fs::write(dir.join("n.ts"), src2).unwrap();
        let syms2 = sc.symbols(&dir, "n.ts").unwrap();
        let by2 = |name: &str, kind: &str| syms2.iter().any(|s| s.name == name && s.kind == kind);
        assert!(by2("default", "function"), "anonymous default export named 'default'; got {syms2:?}");
        assert!(by2("Widget", "class"), "class expression assigned to const captured; got {syms2:?}");
        assert!(by2("constructor", "constructor"), "constructor captured; got {syms2:?}");
        assert!(by2("render", "method"), "method inside the class-expression captured; got {syms2:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The C sidecar (`resolve-c.mjs`) speaks the same protocol: it resolves the `#include`
    /// graph (incl. `<angle>` headers that map into the repo) and slices a function across
    /// `.c` files via the repo-wide definition index.
    #[test]
    fn c_sidecar_imports_and_cross_file_slice() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar/resolve-c.mjs");
        let mut sc = match Sidecar::spawn(&script) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping C sidecar test: node not available ({e})");
                return;
            }
        };
        assert_eq!(sc.health().unwrap()["lang"], "c");

        let dir = tmp();
        fs::create_dir_all(dir.join("include")).unwrap();
        fs::write(dir.join("include/util.h"), "int helper(int x);\n").unwrap();
        fs::write(dir.join("util.c"), "#include \"include/util.h\"\nint helper(int x) {\n  return x * 2;\n}\n").unwrap();
        fs::write(dir.join("main.c"), "#include <util.h>\nint compute(int n) {\n  return helper(n) + 1;\n}\n").unwrap();

        // include graph: `<util.h>` resolves into the repo's include/ dir
        let imps = sc.imports(&dir, "main.c").unwrap();
        assert!(imps.iter().any(|t| t == "include/util.h"), "resolved <util.h>; got {imps:?}");

        // cross-file slice: compute pulls in helper — DEFINED in util.c, only declared in the
        // header — proving definition-index resolution across .c files (the C challenge).
        let defs = sc.slice(&dir, "main.c", "compute", 2).unwrap();
        assert!(defs.iter().any(|d| d.symbol == "compute" && d.file == "main.c"), "target in slice; got {defs:?}");
        let helper = defs.iter().find(|d| d.symbol == "helper").expect("helper pulled cross-file");
        assert_eq!(helper.file, "util.c", "resolved to the DEFINITION in util.c, not the header decl");
        assert!(helper.text.contains("x * 2"), "slice carries helper's body");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The Go sidecar (`resolve-go.mjs`): resolves intra-module imports (via go.mod) to a
    /// package's `.go` files and slices funcs across files/packages via the definition index.
    #[test]
    fn go_sidecar_imports_and_cross_package_slice() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar/resolve-go.mjs");
        let mut sc = match Sidecar::spawn(&script) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping Go sidecar test: node not available ({e})");
                return;
            }
        };
        if sc.health().unwrap().get("treeSitter").and_then(|v| v.as_bool()) != Some(true) {
            eprintln!("skipping Go sidecar test: tree-sitter-go not installed");
            return;
        }
        let dir = tmp();
        fs::write(dir.join("go.mod"), "module mymod\n\ngo 1.21\n").unwrap();
        fs::create_dir_all(dir.join("util")).unwrap();
        fs::write(dir.join("util/util.go"), "package util\n\nfunc Helper(x int) int { return x * 2 }\n").unwrap();
        fs::write(dir.join("main.go"), "package main\n\nimport \"mymod/util\"\n\nfunc Compute(n int) int { return util.Helper(n) + 1 }\n").unwrap();

        // intra-module import → the util package's .go files
        let imps = sc.imports(&dir, "main.go").unwrap();
        assert!(imps.iter().any(|t| t == "util/util.go"), "resolved mymod/util → util/util.go; got {imps:?}");

        // cross-package slice: Compute → util.Helper (selector call resolved via def index)
        let defs = sc.slice(&dir, "main.go", "Compute", 2).unwrap();
        assert!(defs.iter().any(|d| d.symbol == "Compute" && d.file == "main.go"), "target; got {defs:?}");
        let helper = defs.iter().find(|d| d.symbol == "Helper").expect("Helper pulled cross-package");
        assert_eq!(helper.file, "util/util.go");
        assert!(helper.text.contains("x * 2"), "slice carries Helper's body");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The Python sidecar (`resolve-py.mjs`): resolves the import graph (relative + absolute
    /// to in-repo `.py`) and slices a function across modules via the definition index.
    #[test]
    fn py_sidecar_imports_and_cross_file_slice() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar/resolve-py.mjs");
        let mut sc = match Sidecar::spawn(&script) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping Python sidecar test: node not available ({e})");
                return;
            }
        };
        if sc.health().unwrap().get("treeSitter").and_then(|v| v.as_bool()) != Some(true) {
            eprintln!("skipping Python sidecar test: tree-sitter-python not installed");
            return;
        }
        let dir = tmp();
        fs::write(dir.join("util.py"), "def helper(x):\n    return x * 2\n").unwrap();
        fs::write(dir.join("main.py"), "from util import helper\n\ndef compute(n):\n    return helper(n) + 1\n").unwrap();

        // import graph: main.py → util.py (absolute import from repo root)
        let imps = sc.imports(&dir, "main.py").unwrap();
        assert!(imps.iter().any(|t| t == "util.py"), "resolved `from util` → util.py; got {imps:?}");

        // cross-module slice: compute pulls in helper (defined in util.py)
        let defs = sc.slice(&dir, "main.py", "compute", 2).unwrap();
        assert!(defs.iter().any(|d| d.symbol == "compute" && d.file == "main.py"), "target; got {defs:?}");
        let helper = defs.iter().find(|d| d.symbol == "helper").expect("helper pulled cross-module");
        assert_eq!(helper.file, "util.py");
        assert!(helper.text.contains("x * 2"), "slice carries helper's body");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A sidecar that reads but never answers must NOT hang the caller: `call` returns a
    /// TimedOut error, the sidecar is marked dead, and the next call fails fast. This is
    /// the resolver analogue of the daemon's slow-client protection.
    #[test]
    fn wedged_sidecar_times_out_instead_of_hanging() {
        let dir = tmp();
        // A "sidecar" that consumes stdin forever and never writes a response.
        let script = dir.join("wedged.mjs");
        fs::write(&script, "process.stdin.resume();\nsetInterval(() => {}, 1 << 30);\n").unwrap();

        let mut sc = match Sidecar::spawn(&script) {
            Ok(s) => s.with_timeout(Duration::from_millis(300)),
            Err(e) => {
                eprintln!("skipping wedged-sidecar test: node not available ({e})");
                return;
            }
        };
        let start = std::time::Instant::now();
        let err = sc.health().expect_err("wedged sidecar must not answer");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "timed out (got {err:?})");
        assert!(start.elapsed() < Duration::from_secs(5), "returned promptly, didn't hang");
        // dead → the next call fails fast, it does not block for another timeout
        let t2 = std::time::Instant::now();
        let err2 = sc.imports(&dir, "x.ts").expect_err("dead sidecar rejects further calls");
        assert_eq!(err2.kind(), io::ErrorKind::BrokenPipe, "fails fast as dead (got {err2:?})");
        assert!(t2.elapsed() < Duration::from_millis(100), "fast-fail, no second timeout wait");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A *crashed* sidecar (process exited) heals itself: the next call respawns a fresh
    /// child. Fixture script crashes on its first run, then behaves — so the first call
    /// errors (EOF) and the second, after auto-respawn, succeeds.
    #[test]
    fn crashed_sidecar_auto_respawns_and_recovers() {
        let dir = tmp();
        let script = dir.join("crash-once.mjs");
        fs::write(
            &script,
            r#"import fs from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import readline from "node:readline";
const here = path.dirname(fileURLToPath(import.meta.url));
const marker = path.join(here, ".crashed_once");
if (!fs.existsSync(marker)) { fs.writeFileSync(marker, "1"); process.exit(1); }
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const req = JSON.parse(line);
  process.stdout.write(JSON.stringify({ id: req.id, ok: true, result: { lang: "revived" } }) + "\n");
});
"#,
        )
        .unwrap();

        let mut sc = match Sidecar::spawn(&script) {
            Ok(s) => s.with_timeout(Duration::from_secs(5)),
            Err(e) => {
                eprintln!("skipping crash-recovery test: node not available ({e})");
                return;
            }
        };
        // first run crashes (exit 1) before answering → detected as a crash (EOF), not a wedge
        let e1 = sc.health().expect_err("first run crashes");
        assert_eq!(e1.kind(), io::ErrorKind::UnexpectedEof, "crash detected (got {e1:?})");
        // second call auto-respawns; the fresh child now behaves → success
        let h = sc.health().expect("respawn heals the crashed sidecar");
        assert_eq!(h["lang"], "revived", "recovered via respawn");

        let _ = fs::remove_dir_all(&dir);
    }
}
