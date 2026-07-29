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
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// One definition in a symbol slice: a function-like declaration with its repo-relative
/// `file`, `symbol` name, and source `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceDef {
    pub file: String,
    pub symbol: String,
    pub text: String,
}

pub struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Sidecar {
    /// Spawn `node <script>` as a resolver sidecar. Errors if `node` isn't on PATH.
    pub fn spawn(script: &Path) -> io::Result<Sidecar> {
        let mut child = Command::new("node")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        Ok(Sidecar { child, stdin, stdout, next_id: 0 })
    }

    fn call(&mut self, mut req: Value) -> io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        let line =
            serde_json::to_string(&req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut resp = String::new();
        if self.stdout.read_line(&mut resp)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "resolver sidecar closed"));
        }
        let v: Value = serde_json::from_str(resp.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if v.get("id").and_then(Value::as_u64) != Some(id) {
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
}
