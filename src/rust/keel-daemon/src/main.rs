//! keeld — the crash-only keel daemon.
//!
//! Holds the object store + live graph + coordination warm, so `keel brief` doesn't rebuild
//! the graph every call. Listens on `<root>/.keel/daemon.sock` (Unix), newline-delimited JSON,
//! one request per connection. A connection is served on its own thread (so one slow/stalled
//! client can't freeze the daemon), but the number of connections in flight at once is
//! **bounded** (a std semaphore) so a connection flood can't exhaust threads — excess
//! connections wait in the listener backlog. The brief handling itself is serialized behind a
//! mutex (LMDB is single-writer; one sidecar). On each `brief` it refreshes the graph
//! incrementally (cheap — only changed files) so it stays live to the working tree.
//!
//! Ops: `{op:"ping"}` · `{op:"brief", task, file, symbol?, budget?, reserve?}`.

use keel_brief::BriefService;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Max connections served concurrently. Brief handling serializes behind the service mutex,
/// so extra concurrency buys no throughput — this cap exists purely to bound thread growth
/// under a connection flood. When it's reached, `accept` simply pauses (new connections queue
/// in the OS backlog) until a slot frees.
const MAX_INFLIGHT: usize = 128;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = flag(&args, "--root")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .expect("cwd");
    let store = flag(&args, "--store").map(PathBuf::from).unwrap_or_else(|| root.join(".keel/store"));
    // resolver sidecar DIR (holds resolve.mjs / resolve-c.mjs). --resolver/KEEL_RESOLVER may
    // name the dir or a script inside it (parent is used, for backward compatibility).
    let resolver = match flag(&args, "--resolver")
        .map(PathBuf::from)
        .or_else(|| std::env::var("KEEL_RESOLVER").ok().map(PathBuf::from))
    {
        Some(p) if p.is_dir() => p,
        Some(p) => p.parent().map(Path::to_path_buf).unwrap_or(p),
        None => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../keel-resolve/sidecar")),
    };

    let sock = root.join(".keel/daemon.sock");
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&sock); // clear a stale socket (crash-only recovery)

    eprintln!("keeld: opening store + building graph ...");
    let mut svc = match BriefService::open(&root, &store, &resolver) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("keeld: failed to open: {e}");
            std::process::exit(1);
        }
    };
    // fs-watch so each brief's refresh is O(changed), not a full-tree walk. Non-fatal: if it
    // fails, refresh just keeps using its full-walk fallback.
    match svc.watch() {
        Ok(()) => eprintln!("keeld: fs-watching for incremental refresh"),
        Err(e) => eprintln!("keeld: fs-watch unavailable ({e}); refresh will walk the tree"),
    }
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("keeld: cannot bind {}: {e}", sock.display());
            std::process::exit(1);
        }
    };
    eprintln!("keeld: warm, listening on {}", sock.display());

    // Thread-per-connection so one slow/stalled client can't freeze the daemon; each
    // connection carries a read timeout so a stalled reader is dropped, not leaked. The
    // brief handling itself is serialized behind a mutex (one sidecar / single writer),
    // and the lock is taken AFTER the (possibly slow) read — so a stalled reader never
    // holds it.
    //
    // Concurrency is bounded by a token semaphore built from a bounded channel: it starts
    // full with MAX_INFLIGHT tokens; the accept loop takes one before spawning a worker
    // (blocking, so a flood applies back-pressure instead of spawning unbounded threads),
    // and the worker returns it when the connection is done.
    let svc = Arc::new(Mutex::new(svc));
    let (slot_tx, slot_rx) = sync_channel::<()>(MAX_INFLIGHT);
    for _ in 0..MAX_INFLIGHT {
        slot_tx.send(()).expect("prefill semaphore");
    }
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("keeld: accept error: {e}");
                continue;
            }
        };
        // acquire a slot — blocks (back-pressure) once MAX_INFLIGHT are in flight
        if slot_rx.recv().is_err() {
            break; // semaphore closed — shutting down
        }
        let svc = Arc::clone(&svc);
        let slot_tx = slot_tx.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            if let Err(e) = serve_one(&svc, stream) {
                eprintln!("keeld: connection dropped: {e}");
            }
            let _ = slot_tx.send(()); // release the slot
        });
    }
    let _ = std::fs::remove_file(&sock);
}

fn serve_one(svc: &Mutex<BriefService>, stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client closed (or timed out) without a request
    }
    let req: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({}));
    let resp = {
        let mut svc = svc.lock().unwrap();
        handle(&mut svc, &req)
    };
    let mut stream = stream;
    stream.write_all(serde_json::to_string(&resp)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Dispatch one request against the warm service.
pub fn handle(svc: &mut BriefService, req: &Value) -> Value {
    match req.get("op").and_then(Value::as_str) {
        Some("ping") => json!({"ok": true, "pong": true}),
        Some("brief") => {
            let Some(file) = req.get("file").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "file is required"});
            };
            let task = req.get("task").and_then(Value::as_str).unwrap_or("(unspecified)");
            let symbol = req.get("symbol").and_then(Value::as_str);
            let budget = req.get("budget").and_then(Value::as_u64).unwrap_or(8000) as usize;
            let reserve = req.get("reserve").and_then(Value::as_bool).unwrap_or(false);
            // per-request agent id — the shared coordinator evaluates reservations/predictions
            // against THIS agent, so many agents coordinate through the one warm daemon.
            svc.set_agent(req.get("agent").and_then(Value::as_str).unwrap_or("local"));
            // keep the graph live: incremental refresh (only changed files) before answering
            if let Err(e) = svc.refresh() {
                return json!({"ok": false, "error": format!("refresh: {e}")});
            }
            match svc.brief(task, file, symbol, budget, reserve) {
                Ok(b) => {
                    let mut v = b.to_json();
                    v["ok"] = json!(true);
                    v
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
        other => json!({"ok": false, "error": format!("unknown op: {other:?}")}),
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}
