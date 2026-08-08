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
//! `{op:"status"}` (warm, O(changed) working-tree status via an fs-watch-fed [`LiveStatus`]) ·
//! `{op:"announce", event}` (broadcast a coordination event to the fleet) ·
//! `{op:"reservations"}` (list every active hold in the shared coordinator) ·
//! `{op:"release", agent, files?}` (free an agent's holds — the named files, or all of them).
//!
//! With `--quic [host:port]` the daemon also runs a QUIC coordination server (shared store): every
//! brief broadcasts an "agent working on file X" presence event and `announce` pushes lessons /
//! landed changes, so remote agents subscribed via `keel net-events` coordinate live over the wire.

use keel_brief::BriefService;
use keel_store::LiveStatus;
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
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
    coord: Option<keel_net::Publisher>, // broadcasts fleet-coordination events over QUIC
}

/// Start a QUIC coordination server on a background tokio runtime and return a synchronous
/// [`Publisher`](keel_net::Publisher). The server shares the daemon's object store (an Arc clone —
/// NOT a second LMDB open) so remote agents can fetch objects and subscribe to the same event
/// stream. `addr` is `host:port` (`0` picks a free port); the runtime thread lives for the
/// daemon's lifetime.
fn start_quic(addr: &str, store: keel_store::Store) -> Option<keel_net::Publisher> {
    // Accept "host:port" or a bare port (bound on loopback — a public bind needs an explicit host).
    let sockaddr: std::net::SocketAddr = addr
        .parse()
        .or_else(|_| addr.parse::<u16>().map(|p| ([127, 0, 0, 1], p).into()))
        .ok()?;
    let (tx, rx) = sync_channel::<Result<(keel_net::Publisher, std::net::SocketAddr), String>>(1);
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            match keel_net::Server::bind(sockaddr, store).await {
                Ok(server) => {
                    let _ = tx.send(Ok((server.publisher(), server.local_addr())));
                    std::future::pending::<()>().await; // keep the runtime + server alive
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        });
    });
    match rx.recv() {
        Ok(Ok((publisher, bound))) => {
            eprintln!("keeld: QUIC coordination serving on {bound}");
            Some(publisher)
        }
        Ok(Err(e)) => {
            eprintln!("keeld: QUIC coordination disabled (bind failed: {e})");
            None
        }
        Err(_) => {
            eprintln!("keeld: QUIC coordination disabled (runtime did not start)");
            None
        }
    }
}

/// Max connections served concurrently. Brief handling serializes behind the service mutex,
/// so extra concurrency buys no throughput — this cap exists purely to bound thread growth
/// under a connection flood. When it's reached, `accept` simply pauses (new connections queue
/// in the OS backlog) until a slot frees.
const MAX_INFLIGHT: usize = 128;

/// Hard cap on a single request read off the socket. `read_line` grows until a newline, so an
/// unbounded stream with no `\n` would let one client OOM the whole daemon; requests are small
/// JSON objects, so 1 MiB is generous.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Returns a concurrency slot to the semaphore when dropped — even on a worker panic/unwind.
struct SlotGuard(std::sync::mpsc::SyncSender<()>);
impl Drop for SlotGuard {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Lock a mutex, tolerating poisoning. A panic while a lock is held would otherwise poison it and
/// make every later `.lock().unwrap()` panic — turning one bad request into a permanent daemon DoS.
/// The protected state is plain data (graph/status caches), so recovering the inner value is safe.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

    // Shared with the CLI so both agree on the path even when it falls back off the long repo path.
    let sock = keel_store::daemon_socket_path(&root);
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

    // Reload reservations persisted by a previous daemon so a restart doesn't drop everyone's holds
    // (expired ones are discarded, survivors keep their remaining TTL). Best-effort.
    let restored = svc.restore_holds(&load_holds(&root), now_secs());
    if restored > 0 {
        eprintln!("keeld: restored {restored} reservation(s) from coord.json");
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
    // Optional QUIC coordination server: `--quic [host:port]`. Defaults to loopback (127.0.0.1:0):
    // the server is currently unauthenticated (no client auth / cert pinning), so binding all
    // interfaces by default would expose the fleet event stream and object fetch to the network.
    // An explicit `--quic 0.0.0.0:PORT` still opts into a public bind.
    let coord = if args.iter().any(|a| a == "--quic") {
        let addr = flag(&args, "--quic").unwrap_or("127.0.0.1:0");
        start_quic(addr, svc.repo().store().clone())
    } else {
        None
    };

    let ctx = Arc::new(Ctx {
        svc: Mutex::new(svc),
        live,
        pending,
        _status_watcher: Mutex::new(status_watcher),
        coord,
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
        // Release the slot from a Drop guard, so it is returned even if the worker *panics*.
        // Without this, a panicking handler would leak its slot; after MAX_INFLIGHT panics the
        // accept loop would block forever on `slot_rx.recv()` and the daemon would wedge.
        let slot = SlotGuard(slot_tx.clone());
        std::thread::spawn(move || {
            let _slot = slot; // dropped (slot released) on any exit, including unwind
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            // Contain a handler panic to this connection instead of poisoning shared state and
            // tearing down the thread mid-response; the slot still releases via `_slot`.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve_one(&ctx, stream)));
            match result {
                Ok(Err(e)) => eprintln!("keeld: connection dropped: {e}"),
                Err(_) => eprintln!("keeld: connection handler panicked (contained)"),
                Ok(Ok(())) => {}
            }
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
        let mut p = lock(&sink);
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
    // Bound the read: a client that never sends `\n` can't grow `line` past the cap and OOM us.
    if reader.by_ref().take(MAX_REQUEST_BYTES).read_line(&mut line)? == 0 {
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
        let mut p = lock(&ctx.pending);
        (std::mem::take(&mut p.paths), std::mem::replace(&mut p.full_rescan, false))
    };
    {
        let svc = lock(&ctx.svc);
        let mut l = lock(live);
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
    let changes: Vec<Value> = lock(live)
        .changes()
        .into_iter()
        .map(|c| json!({"path": c.path, "status": c.kind.marker().to_string()}))
        .collect();
    json!({"ok": true, "changes": changes})
}

/// Wall-clock seconds since the epoch, for stamping coordination events.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where reservations are persisted (per-repo), so a restart can reload them.
fn coord_path(root: &Path) -> PathBuf {
    root.join(".keel").join("coord.json")
}

/// Persist the coordinator's holds to disk. Best-effort: a failed write just means a restart forgets
/// them (they'd expire on TTL anyway), so errors are swallowed rather than failing the op.
fn persist_holds(root: &Path, records: &[keel_brief::HoldRecord]) {
    let arr: Vec<Value> = records
        .iter()
        .map(|h| json!({"file": h.file, "agent": h.agent, "task": h.task, "taken_unix": h.taken_unix}))
        .collect();
    let path = coord_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(s) = serde_json::to_string(&arr) else { return };
    // Atomic replace: write a sibling temp then rename, so a crash mid-write can't leave a torn file
    // that fails to parse and drops ALL holds on the next restart.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &s).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Read persisted holds — empty if the file is missing, unreadable, or malformed (a corrupt file
/// must not stop the daemon from starting).
fn load_holds(root: &Path) -> Vec<keel_brief::HoldRecord> {
    let Ok(text) = std::fs::read_to_string(coord_path(root)) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            Some(keel_brief::HoldRecord {
                file: it.get("file")?.as_str()?.to_string(),
                agent: it.get("agent")?.as_str()?.to_string(),
                task: it.get("task")?.as_str()?.to_string(),
                taken_unix: it.get("taken_unix")?.as_u64()?,
            })
        })
        .collect()
}

/// Broadcast a coordination event over QUIC (no-op if the server isn't running / no subscribers).
fn broadcast(ctx: &Ctx, event: &Value) {
    if let Some(pubr) = &ctx.coord {
        if let Ok(bytes) = serde_json::to_vec(event) {
            pubr.publish(bytes);
        }
    }
}

/// Dispatch one request against the warm service.
fn handle(ctx: &Ctx, req: &Value) -> Value {
    match req.get("op").and_then(Value::as_str) {
        Some("ping") => json!({"ok": true, "pong": true}),
        Some("status") => status_response(ctx),
        // Push an arbitrary coordination event to the fleet — e.g. `keel learn` / `keel commit`
        // announcing a lesson or a landed change so subscribed agents see it live.
        Some("announce") => {
            let Some(event) = req.get("event") else {
                return json!({"ok": false, "error": "event is required"});
            };
            let mut event = event.clone();
            if let Value::Object(m) = &mut event {
                m.entry("ts").or_insert(json!(now_secs()));
            }
            broadcast(ctx, &event);
            json!({"ok": true, "broadcast": ctx.coord.is_some()})
        }
        Some("brief") => {
            let Some(file) = req.get("file").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "file is required"});
            };
            let task = req.get("task").and_then(Value::as_str).unwrap_or("(unspecified)");
            let symbol = req.get("symbol").and_then(Value::as_str);
            let budget = req.get("budget").and_then(Value::as_u64).unwrap_or(8000) as usize;
            let reserve = req.get("reserve").and_then(Value::as_bool).unwrap_or(false);
            let agent = req.get("agent").and_then(Value::as_str).unwrap_or("local").to_string();
            // Fleet presence: broadcast that this agent is about to work on `file`, so other
            // agents/dashboards see who's working where in real time (coordination, not just data).
            broadcast(ctx, &json!({"kind": "brief", "agent": agent, "file": file, "task": task, "ts": now_secs()}));
            // Capture the response and (if holds changed) what to persist, then write AFTER releasing
            // the service lock so a slow disk doesn't serialize every other agent's requests.
            let (resp, persist) = {
                let mut svc = lock(&ctx.svc);
                let svc = &mut *svc;
                // per-request agent id — the shared coordinator evaluates reservations/predictions
                // against THIS agent, so many agents coordinate through the one warm daemon.
                svc.set_agent(&agent);
                // keep the graph live: incremental refresh (only changed files) before answering
                if let Err(e) = svc.refresh() {
                    return json!({"ok": false, "error": format!("refresh: {e}")});
                }
                match svc.brief(task, file, symbol, budget, reserve) {
                    Ok(b) => {
                        let mut v = b.to_json();
                        v["ok"] = json!(true);
                        // EVERY brief heartbeats this agent's holds (refreshing their timestamps), so
                        // persist whenever the agent holds anything — not just on --reserve. Otherwise a
                        // reserve-once-then-plain-brief workflow would leave coord.json's timestamps
                        // frozen at the first reserve and a later restart would wrongly age the hold out.
                        let records = svc.export_holds();
                        let persist = (reserve || !records.is_empty()).then(|| (svc.root().to_path_buf(), records));
                        (v, persist)
                    }
                    Err(e) => (json!({"ok": false, "error": e.to_string()}), None),
                }
            };
            if let Some((root, records)) = persist {
                persist_holds(&root, &records);
            }
            resp
        }
        // Read-only observability: every active hold in the shared coordinator.
        Some("reservations") => {
            let svc = lock(&ctx.svc);
            let held: Vec<Value> = svc
                .reservations()
                .into_iter()
                .map(|h| {
                    json!({
                        "file": h.file, "agent": h.agent, "task": h.task,
                        "age_secs": h.age_secs, "ttl_remaining_secs": h.ttl_remaining_secs,
                    })
                })
                .collect();
            json!({"ok": true, "reservations": held})
        }
        // Release an agent's holds: the named `files` if given, else all of them.
        Some("release") => {
            let Some(agent) = req.get("agent").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "agent is required"});
            };
            let files: Vec<String> = req
                .get("files")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let (freed, persist) = {
                let svc = lock(&ctx.svc);
                let freed = svc.release(agent, &files);
                // reflect the freed holds on disk (outside the lock); only if something changed
                let persist = (freed > 0).then(|| (svc.root().to_path_buf(), svc.export_holds()));
                (freed, persist)
            };
            if let Some((root, records)) = persist {
                persist_holds(&root, &records);
            }
            json!({"ok": true, "released": freed})
        }
        other => json!({"ok": false, "error": format!("unknown op: {other:?}")}),
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("keeld-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(file: &str, agent: &str, task: &str, taken_unix: u64) -> keel_brief::HoldRecord {
        keel_brief::HoldRecord { file: file.into(), agent: agent.into(), task: task.into(), taken_unix }
    }

    #[test]
    fn persist_then_load_round_trips_holds() {
        let root = tmpdir("persist");
        let recs = vec![rec("a.ts", "al", "t1", 100), rec("dir/b.ts", "bo", "t2", 200)];
        persist_holds(&root, &recs);
        assert_eq!(load_holds(&root), recs);
        // a rewrite fully replaces the previous set (atomic rename leaves no stale temp)
        persist_holds(&root, &recs[..1]);
        assert_eq!(load_holds(&root), recs[..1]);
        assert!(!coord_path(&root).with_extension("json.tmp").exists(), "temp file cleaned up");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persist_round_trips_weird_strings() {
        let root = tmpdir("weird");
        let recs = vec![rec("d/\"q\" and\nnewline.ts", "agent 🌊", "fix \"it\"", 42)];
        persist_holds(&root, &recs);
        assert_eq!(load_holds(&root), recs, "unicode/quotes/newlines survive json round-trip");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_holds_tolerates_missing_and_corrupt() {
        let root = tmpdir("corrupt");
        assert!(load_holds(&root).is_empty(), "missing file → empty");
        let path = coord_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_holds(&root).is_empty(), "garbage → empty, no panic");
        std::fs::write(&path, "[{\"file\":\"a.ts\"}]").unwrap();
        assert!(load_holds(&root).is_empty(), "items missing required fields are skipped");
        let _ = std::fs::remove_dir_all(&root);
    }
}
