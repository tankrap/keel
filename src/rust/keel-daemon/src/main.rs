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
//! Ops: `{op:"ping"}` · `{op:"brief", task, file, symbol?, budget?, reserve?}` ·
//! `{op:"status"}` (warm, O(changed) working-tree status via an fs-watch-fed [`LiveStatus`]).

use keel_brief::BriefService;
use keel_store::LiveStatus;
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Filesystem changes the status watcher has seen but `LiveStatus` hasn't folded in yet. Drained
/// on each `status` request (pull model) so an unread event stream can't grow unbounded and the
/// watcher callback never has to lock `LiveStatus`.
#[derive(Default)]
struct Pending {
    paths: Vec<PathBuf>,
    full_rescan: bool,
}

/// Everything a connection handler needs. `svc` (the brief service) and `live` (warm status) each
/// have their own lock; the status path takes `svc` then `live` (never the reverse), and the
/// watcher callback touches only `pending` — so the two never deadlock.
struct Ctx {
    svc: Mutex<BriefService>,
    live: Option<Mutex<LiveStatus>>,
    pending: Arc<Mutex<Pending>>, // shared with the status watcher's callback
    _status_watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

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

    // Warm working-tree status: seed once, then an fs-watch feeds changed paths so `status` over
    // the socket is O(changed), not a full walk. Non-fatal — a failure just leaves `status`
    // reporting unavailable, and callers fall back to the local walk.
    let (live, pending, status_watcher) = match LiveStatus::seed(svc.repo(), svc.root()) {
        Ok(l) => {
            let pending = Arc::new(Mutex::new(Pending::default()));
            let watcher = start_status_watch(svc.root(), &pending);
            match watcher {
                Ok(w) => {
                    eprintln!("keeld: warm status ready (fs-watched)");
                    (Some(Mutex::new(l)), pending, Some(w))
                }
                Err(e) => {
                    eprintln!("keeld: status fs-watch unavailable ({e}); status disabled");
                    (None, pending, None)
                }
            }
        }
        Err(e) => {
            eprintln!("keeld: could not seed status ({e}); status disabled");
            (None, Arc::new(Mutex::new(Pending::default())), None)
        }
    };

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
    let ctx = Arc::new(Ctx {
        svc: Mutex::new(svc),
        live,
        pending,
        _status_watcher: Mutex::new(status_watcher),
    });
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
        let ctx = Arc::clone(&ctx);
        let slot_tx = slot_tx.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            if let Err(e) = serve_one(&ctx, stream) {
                eprintln!("keeld: connection dropped: {e}");
            }
            let _ = slot_tx.send(()); // release the slot
        });
    }
    let _ = std::fs::remove_file(&sock);
}

/// Bind an fs-watcher that pushes changed absolute paths into `pending` (pull model — drained on
/// each `status` request). Mirrors the graph watcher: FSEvents may report `/private/var/…` while
/// the root is `/var/…`, so events carry absolute paths and `LiveStatus` strips the root itself.
fn start_status_watch(root: &Path, pending: &Arc<Mutex<Pending>>) -> notify::Result<notify::RecommendedWatcher> {
    let sink = Arc::clone(pending);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let mut p = sink.lock().unwrap();
        match res {
            Ok(ev) if ev.need_rescan() => p.full_rescan = true,
            Ok(ev) => p.paths.extend(ev.paths),
            Err(_) => p.full_rescan = true,
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn serve_one(ctx: &Ctx, stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client closed (or timed out) without a request
    }
    let req: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({}));
    let resp = handle(ctx, &req);
    let mut stream = stream;
    stream.write_all(serde_json::to_string(&resp)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Fold pending fs events into the warm status and return the current dirty set. Detects a commit
/// (head moved) and re-flattens head; a watcher-signalled rescan re-seeds from a fresh walk.
fn status_response(ctx: &Ctx) -> Value {
    let Some(live) = &ctx.live else {
        return json!({"ok": false, "error": "status unavailable on this daemon"});
    };
    let (paths, rescan) = {
        let mut p = ctx.pending.lock().unwrap();
        (std::mem::take(&mut p.paths), std::mem::replace(&mut p.full_rescan, false))
    };
    {
        let svc = ctx.svc.lock().unwrap();
        let mut l = live.lock().unwrap();
        if rescan {
            match LiveStatus::seed(svc.repo(), svc.root()) {
                Ok(fresh) => *l = fresh,
                Err(e) => return json!({"ok": false, "error": format!("status rescan: {e}")}),
            }
        } else {
            // a commit moved head → re-flatten it before folding the file changes
            if svc.repo().head().ok().flatten() != l.head_id() {
                if let Err(e) = l.refresh_head(svc.repo()) {
                    return json!({"ok": false, "error": format!("status head refresh: {e}")});
                }
            }
            l.on_paths(&paths);
        }
    }
    let changes: Vec<Value> = live
        .lock()
        .unwrap()
        .changes()
        .into_iter()
        .map(|c| json!({"path": c.path, "status": c.kind.marker().to_string()}))
        .collect();
    json!({"ok": true, "changes": changes})
}

/// Dispatch one request against the warm service.
fn handle(ctx: &Ctx, req: &Value) -> Value {
    match req.get("op").and_then(Value::as_str) {
        Some("ping") => json!({"ok": true, "pong": true}),
        Some("status") => status_response(ctx),
        Some("brief") => {
            let mut svc = ctx.svc.lock().unwrap();
            let svc = &mut *svc;
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
