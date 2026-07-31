//! keel — the agent-facing CLI.
//!
//! `keel brief --file <f> [--symbol <s>] [--task <t>] [--budget N] [--json] [--reserve]`
//! runs the fused fetch: context (symbol slice) + graph (deps/rdeps) + coordination +
//! provenance + relevant prior sessions, in ONE call. If a `keeld` daemon is running for
//! the repo it answers (warm — no graph rebuild); otherwise the CLI runs it in-process.
//! `keel commit` / `keel log` are the write/history side.

use keel_brief::BriefService;
use keel_store::{diff_lines, ChangeKind, Object, ObjectId, Repo, Session, StoreError, Tag, Verification};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // keel is a drop-in for git: its own value-add verbs (below) take precedence, `init`/`clone`
    // are git + keel-mirror, and every other verb passes straight through to the real git binary
    // (with a mirror re-sync after mutating commands). So `keel <anything git understands>` works.
    match args.first().map(String::as_str) {
        // ── keel value-add surface (the fused graph + git mirror) ──
        Some("brief") => run(cmd_brief(&args[1..])),
        Some("repack") => run(cmd_repack(&args[1..])),
        Some("size") => run(cmd_size(&args[1..])),
        Some("import") => run(cmd_import(&args[1..])),
        Some("export") => run(cmd_export(&args[1..])),
        Some("mirror-in") => run(cmd_mirror_in(&args[1..])),
        Some("mirror-out") => run(cmd_mirror_out(&args[1..])),
        Some("reindex") => run(cmd_reindex(&args[1..])),
        Some("serve") => run(cmd_serve(&args[1..])),
        Some("net-serve") => run(cmd_net_serve(&args[1..])),
        Some("verify") => run(cmd_verify(&args[1..])),
        Some("pin") => run(cmd_pin(&args[1..])),
        Some("pins") => run(cmd_pins(&args[1..])),
        Some("sessions") => run(cmd_sessions(&args[1..])),
        Some("session") => run(cmd_session(&args[1..])),
        Some("learn") => run(cmd_learn(&args[1..])),
        Some("native") => run(cmd_native(&args[1..])),
        // ── git-compatible surface ──
        Some("init") => run(cmd_init(&args[1..])),   // git init + keel store
        Some("clone") => run(cmd_clone(&args[1..])), // git clone + keel mirror
        Some("help") | None => print_usage(),
        // everything else IS a git command → forward to git (with mirror sync)
        Some(_) => git_passthrough(&args),
    }
}

fn run(r: io::Result<()>) {
    if let Err(e) = r {
        eprintln!("keel: {e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!(
        "keel — agent-native VCS, drop-in compatible with git\n\n\
         GIT-COMPATIBLE: any git command works as a keel command —\n\
         \x20 keel clone <url> [dir]      git clone + start a keel mirror\n\
         \x20 keel init  [dir]            git init  + a keel store alongside\n\
         \x20 keel add / commit / status / log / diff / branch / checkout / merge /\n\
         \x20 keel push / pull / fetch / remote / tag / rebase / stash / ...\n\
         \x20   → forwarded to git verbatim; mutating commands re-sync the keel mirror.\n\n\
         KEEL VALUE-ADD (the fused graph + git mirror):\n\
         \x20 keel brief  --file <path> [--symbol <name>] [--task <t>] [--json] [--reserve]\n\
         \x20 keel pin <symbol> --lesson <text>   ·   keel pins\n\
         \x20 keel learn --lesson <text> [--task <text>]   record what a change taught (flywheel)\n\
         \x20 keel sessions [--file <path>]       ·   keel session <change>\n\
         \x20 keel repack [--json]                delta-compress history + GC (like `git gc`)\n\
         \x20 keel size   [--json]                logical bytes / object counts\n\
         \x20 keel mirror-in <git-repo>  ·  keel mirror-out <dir>  ·  keel import/export\n\
         \x20 keel verify <change> --green|--red\n\
         \x20 keel native <commit|status|log|diff>   keel's own store-backed operations\n\n\
         A running `keeld` for the repo answers briefs warm; otherwise runs in-process."
    );
}

fn cmd_init(args: &[String]) -> io::Result<()> {
    // git init first (drop-in), passing through any positional dir / git flags, then set up the
    // keel store alongside so the repo is both git- and keel-tracked from the start.
    let status = std::process::Command::new("git").arg("init").args(args).status();
    match status {
        Ok(s) if s.success() => {}
        Ok(_) => std::process::exit(1),
        Err(e) => eprintln!("keel: git init unavailable ({e}); setting up keel store only"),
    }
    let (root, store) = root_store(args)?;
    let existing = std::fs::read_dir(&store).map(|mut d| d.next().is_some()).unwrap_or(false);
    if !existing {
        Repo::open(&store).map_err(to_io)?; // opening creates the LMDB store on disk
    }
    exclude_keel(&root);
    println!("initialized keel repo (git + keel)");
    println!("  root:     {}", root.display());
    println!("  store:    {}", store.display());
    Ok(())
}

fn cmd_brief(args: &[String]) -> io::Result<()> {
    let file = flag(args, "--file").ok_or_else(|| io::Error::other("--file is required"))?;
    let symbol = flag(args, "--symbol");
    let task = flag(args, "--task").unwrap_or("(unspecified)");
    let budget: usize = flag(args, "--budget").and_then(|s| s.parse().ok()).unwrap_or(8_000);
    let reserve = has(args, "--reserve");
    let json = has(args, "--json");
    let agent = flag(args, "--agent").unwrap_or("local");
    let (root, store) = root_store(args)?;

    let req = json!({
        "op": "brief", "task": task, "file": file,
        "symbol": symbol, "budget": budget, "reserve": reserve, "agent": agent,
    });

    // Prefer the warm daemon; fall back to in-process.
    let value = match daemon_request(&root, &req) {
        Some(resp) => {
            if resp.get("ok").and_then(Value::as_bool) != Some(true) {
                let msg = resp.get("error").and_then(Value::as_str).unwrap_or("daemon error");
                return Err(io::Error::other(msg.to_string()));
            }
            resp
        }
        None => {
            let mut svc = BriefService::open(&root, &store, &sidecar_dir(args))?.with_agent(agent);
            svc.brief(task, file, symbol, budget, reserve)?.to_json()
        }
    };
    // Record what this brief served, so a subsequent `keel commit --session/--lesson` can
    // auto-link it as the session's context_served — closing the feedback edge
    // (context_served → change → verified) without the agent threading it manually.
    record_last_brief(&root, &value);

    print!("{}", if json { render_json(&value) } else { render_human(&value) });
    Ok(())
}

/// Persist the context this brief served to `<root>/.keel/last_brief.json` (overwrites).
fn record_last_brief(root: &Path, value: &Value) {
    let rec = json!({
        "task": value.get("task").cloned().unwrap_or(Value::Null),
        "file": value.get("file").cloned().unwrap_or(Value::Null),
        "context": value.get("context").cloned().unwrap_or(Value::Null),
    });
    let dir = root.join(".keel");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("last_brief.json"), rec.to_string());
}

/// The most-recent brief's served context (from `.keel/last_brief.json`), stored as a blob —
/// so a session can point its `context_served` at exactly what the agent was shown.
fn last_brief_blob(repo: &Repo, root: &Path) -> Option<ObjectId> {
    let bytes = std::fs::read(root.join(".keel/last_brief.json")).ok()?;
    repo.store().put(&Object::Blob(bytes)).ok()
}

fn cmd_commit(args: &[String]) -> io::Result<()> {
    let msg = flag(args, "--message")
        .or_else(|| flag(args, "-m"))
        .ok_or_else(|| io::Error::other("--message is required"))?;
    let author = flag(args, "--author").unwrap_or("local");
    let (root, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    // Attach an agent session: --session <file.json> captures a full session (transcript,
    // tool calls/results, tokens, lesson, verification); --lesson is the minimal form. Either
    // way, auto-link the last brief's served context (unless the session file set its own).
    let auto_ctx = last_brief_blob(&repo, &root);
    let session_id = if let Some(path) = flag(args, "--session") {
        let mut s = session_from_file(&repo, path, msg, flag(args, "--lesson"))?;
        if s.context_served.is_none() {
            s.context_served = auto_ctx;
        }
        Some(repo.store().put(&Object::Session(s)).map_err(to_io)?)
    } else if let Some(lesson) = flag(args, "--lesson") {
        let s = Session {
            task: msg.to_string(),
            model: "keel-cli".to_string(),
            lesson: lesson.to_string(),
            prompts: None,
            context_served: auto_ctx,
            tool_calls: vec![],
            tool_results: vec![],
            verification: Verification::Unverified,
            tokens_in: 0,
            tokens_out: 0,
        };
        Some(repo.store().put(&Object::Session(s)).map_err(to_io)?)
    } else {
        None
    };
    let change = repo.commit_dir(&root, msg, author, ts, session_id).map_err(to_io)?;
    println!("committed {} · {}", short(&change.to_hex()), msg);
    Ok(())
}

/// Build a `Session` from a JSON capture file (the universal target that per-agent adapters —
/// Claude Code / Cursor / Aider — map their transcripts into). Large payloads (`prompts`,
/// `context_served`, each `tool_calls`/`tool_results` entry) are stored as blobs and the
/// Session holds their ids. Shape:
/// `{ task?, model?, lesson?, prompts?, context_served?, tool_calls?[], tool_results?[],
///    tokens_in?, tokens_out?, verification?: "green"|"red"|"unverified" }`
fn session_from_file(repo: &Repo, path: &str, msg: &str, lesson_override: Option<&str>) -> io::Result<Session> {
    let raw = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| io::Error::other(format!("session json: {e}")))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let blob = |text: &str| -> io::Result<ObjectId> {
        repo.store().put(&Object::Blob(text.as_bytes().to_vec())).map_err(to_io)
    };
    let opt_blob = |k: &str| -> io::Result<Option<ObjectId>> {
        match s(k) {
            Some(t) if !t.is_empty() => Ok(Some(blob(t)?)),
            _ => Ok(None),
        }
    };
    let blob_list = |k: &str| -> io::Result<Vec<ObjectId>> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(blob).collect())
            .unwrap_or_else(|| Ok(Vec::new()))
    };
    let verification = match s("verification") {
        Some("green") => Verification::Green,
        Some("red") => Verification::Red,
        _ => Verification::Unverified,
    };
    Ok(Session {
        task: s("task").unwrap_or(msg).to_string(),
        model: s("model").unwrap_or("unknown").to_string(),
        lesson: lesson_override.or_else(|| s("lesson")).unwrap_or("").to_string(),
        prompts: opt_blob("prompts")?,
        context_served: opt_blob("context_served")?,
        tool_calls: blob_list("tool_calls")?,
        tool_results: blob_list("tool_results")?,
        verification,
        tokens_in: v.get("tokens_in").and_then(Value::as_u64).unwrap_or(0),
        tokens_out: v.get("tokens_out").and_then(Value::as_u64).unwrap_or(0),
    })
}

fn cmd_status(args: &[String]) -> io::Result<()> {
    let (root, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let changes = repo.status(&root).map_err(to_io)?;

    if has(args, "--json") {
        let rows: Vec<Value> = changes
            .iter()
            .map(|c| json!({ "path": c.path, "status": c.kind.marker().to_string() }))
            .collect();
        println!("{}", render_json(&json!({ "changes": rows })));
        return Ok(());
    }

    if changes.is_empty() {
        println!("working tree clean (matches HEAD)");
        return Ok(());
    }
    let (mut a, mut m, mut d) = (0, 0, 0);
    for c in &changes {
        match c.kind {
            keel_store::ChangeKind::Added => a += 1,
            keel_store::ChangeKind::Modified => m += 1,
            keel_store::ChangeKind::Deleted => d += 1,
        }
        println!("  {} {}", c.kind.marker(), c.path);
    }
    println!("{a} added, {m} modified, {d} deleted");
    Ok(())
}

/// `keel mirror-in <git-repo>` — ingest a git repo's whole object DB + refs into the keel store,
/// losslessly (byte-identical git objects, real SHAs recorded). The keel↔git mirror (NEW-1113 M4).
fn cmd_mirror_in(args: &[String]) -> io::Result<()> {
    let src = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel mirror-in <git-repo> [--store <dir>]"))?;
    let (_, store_path) = root_store(args)?;
    let store = keel_store::store::Store::open(&store_path).map_err(to_io)?;
    let stats = ingest_repo(&store, src, false)?;
    let b = keel_git::bridge::bridge(&store)?; // build keel-native history so brief/provenance work
    println!(
        "mirrored git → keel: {} blob · {} tree · {} commit · {} tag  ·  keel graph: {} changes, {} trees  (store {})",
        stats.blobs, stats.trees, stats.commits, stats.tags, b.commits, b.trees, store_path.display()
    );
    Ok(())
}

/// `keel reindex` — rebuild keel's native fused-graph history (Change/Tree DAG) from the git
/// objects already in the mirror. Run after commits made through the git surface so `keel brief`
/// / `keel native log` / provenance see them.
fn cmd_reindex(args: &[String]) -> io::Result<()> {
    let (_, store_path) = root_store(args)?;
    let store = keel_store::store::Store::open(&store_path).map_err(to_io)?;
    // fold in any objects not yet mirrored (e.g. commits made via the git surface), then bridge
    if let Ok(top) = std::env::current_dir() {
        if top.join(".git").exists() {
            let _ = ingest_repo(&store, top.to_str().unwrap_or("."), true);
        }
    }
    let b = keel_git::bridge::bridge(&store)?;
    println!("keel graph reindexed: {} changes, {} trees", b.commits, b.trees);
    Ok(())
}

/// Ingest a git repo's objects + refs into the keel mirror. With `incremental`, only objects the
/// mirror doesn't already have are fetched (cheap re-sync after a git command); otherwise the
/// whole object DB is ingested. Refs are always refreshed.
fn ingest_repo(
    store: &keel_store::store::Store,
    repo: &str,
    incremental: bool,
) -> io::Result<keel_git::mirror::MirrorStats> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // Native path (no git binary): read the repo's loose objects + packfiles + refs directly.
    // Used for a full ingest; the incremental re-sync still uses `git cat-file` to cheaply list
    // only-new oids.
    if !incremental {
        if let Some(git_dir) = keel_git::gitdir::locate(Path::new(repo)) {
            let objs = keel_git::gitdir::read_all_objects(&git_dir)?;
            let stats = keel_git::mirror::ingest_objects(store, &objs)?;
            let refs = keel_git::gitdir::read_refs(&git_dir)?;
            keel_git::mirror::ingest_refs(store, &refs)?;
            return Ok(stats);
        }
    }

    let stats = if incremental {
        // list every oid, keep only the ones we don't already hold
        let check = Command::new("git")
            .args(["-C", repo, "cat-file", "--batch-all-objects", "--batch-check", "--buffer"])
            .output()?;
        if !check.status.success() {
            return Err(io::Error::other("git cat-file --batch-check failed"));
        }
        let mut missing: Vec<u8> = Vec::new();
        for line in check.stdout.split(|&b| b == b'\n') {
            let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
            if parts.len() != 3 {
                continue;
            }
            if let Ok(oid) = keel_git::Oid::from_hex(parts[0]) {
                if !keel_git::mirror::has_object(store, &oid)? {
                    missing.extend_from_slice(parts[0]);
                    missing.push(b'\n');
                }
            }
        }
        if missing.is_empty() {
            keel_git::mirror::MirrorStats::default()
        } else {
            let mut child = Command::new("git")
                .args(["-C", repo, "cat-file", "--batch", "--buffer"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(&missing)?;
            let out = child.wait_with_output()?;
            keel_git::mirror::ingest_batch_stream(store, &out.stdout)?
        }
    } else {
        let batch = Command::new("git")
            .args(["-C", repo, "cat-file", "--batch-all-objects", "--batch", "--buffer"])
            .output()?;
        if !batch.status.success() {
            return Err(io::Error::other(format!("git cat-file failed in {repo} (not a git repo?)")));
        }
        keel_git::mirror::ingest_batch_stream(store, &batch.stdout)?
    };

    ingest_repo_refs(store, repo)?;
    Ok(stats)
}

/// Snapshot a repo's refs (+ symbolic HEAD) into the mirror.
fn ingest_repo_refs(store: &keel_store::store::Store, repo: &str) -> io::Result<()> {
    use std::process::Command;
    let mut refs: Vec<(String, String)> = Vec::new();
    let showref = Command::new("git").args(["-C", repo, "show-ref", "--head"]).output()?;
    for line in String::from_utf8_lossy(&showref.stdout).lines() {
        if let Some((sha, name)) = line.split_once(' ') {
            refs.push((name.to_string(), sha.to_string()));
        }
    }
    if let Ok(h) = Command::new("git").args(["-C", repo, "symbolic-ref", "HEAD"]).output() {
        if h.status.success() {
            let target = String::from_utf8_lossy(&h.stdout).trim().to_string();
            refs.retain(|(n, _)| n != "HEAD");
            refs.push(("HEAD".to_string(), format!("ref: {target}")));
        }
    }
    keel_git::mirror::ingest_refs(store, &refs)?;
    Ok(())
}

/// `keel mirror-out <dir>` — regenerate a byte-identical `.git` from the mirror in the keel
/// store. No dependency on the git binary; every object's oid is re-verified on write.
fn cmd_mirror_out(args: &[String]) -> io::Result<()> {
    let dst = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel mirror-out <dir> [--store <dir>]"))?;
    let (_, store_path) = root_store(args)?;
    let store = keel_store::store::Store::open(&store_path).map_err(to_io)?;
    let stats = keel_git::mirror::materialize(&store, Path::new(dst))?;
    println!(
        "regenerated git ← keel at {dst}: {} blob · {} tree · {} commit · {} tag · {} refs",
        stats.blobs, stats.trees, stats.commits, stats.tags, stats.refs
    );
    Ok(())
}

/// `keel clone <url> [dir]` — `git clone` (all git flags pass through), then set up the keel
/// mirror alongside so the clone is tracked by keel from the first commit.
fn cmd_clone(args: &[String]) -> io::Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    if args.is_empty() {
        return Err(io::Error::other("usage: keel clone <url> [dir] [git-clone-flags…]"));
    }
    // Run git clone, streaming its progress live while capturing stderr so we can read the exact
    // directory it chose from its "Cloning into '<dir>'..." line — robust against any flag/value
    // (e.g. `--depth 1`) that naive positional parsing would misread.
    let mut child =
        Command::new("git").arg("clone").args(args).stderr(Stdio::piped()).spawn()?;
    let mut err = child.stderr.take().unwrap();
    let mut captured = String::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = err.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        io::stderr().write_all(&chunk[..n])?; // live passthrough (incl. \r progress bars)
        captured.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    let status = child.wait()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    let dir = captured
        .find("Cloning into '")
        .and_then(|i| {
            let rest = &captured[i + "Cloning into '".len()..];
            rest.find('\'').map(|j| rest[..j].to_string())
        })
        .or_else(|| {
            // fallback: URL basename minus ".git" (first non-flag arg)
            args.iter().find(|a| !a.starts_with('-')).map(|url| {
                url.rsplit('/').next().unwrap_or(url).trim_end_matches(".git").to_string()
            })
        })
        .ok_or_else(|| io::Error::other("keel clone: could not determine clone directory"))?;
    exclude_keel(Path::new(&dir));
    let store_path = Path::new(&dir).join(".keel/store");
    let store = keel_store::store::Store::open(&store_path).map_err(to_io)?;
    let stats = ingest_repo(&store, &dir, false)?;
    let b = keel_git::bridge::bridge(&store)?; // build keel-native history so brief/provenance work
    println!(
        "keel: tracking clone in {} ({} blob · {} tree · {} commit mirrored · keel graph {} changes)",
        store_path.display(), stats.blobs, stats.trees, stats.commits, b.commits
    );
    // Pre-warm the code graph so the FIRST `keel brief` is fast (this builds + persists the
    // import graph once, instead of the first brief paying for it). Best-effort: needs the Node
    // resolver sidecars, and a failure here never fails the clone.
    eprint!("keel: warming the code graph (one-time index)… ");
    match BriefService::open(Path::new(&dir), &store_path, &sidecar_dir(args)) {
        Ok(_) => eprintln!("done — briefs will be fast."),
        Err(e) => eprintln!("skipped ({e}); the first `keel brief` will warm it."),
    }
    Ok(())
}

/// Ensure a git repo ignores keel's own store, so `keel add -A` never commits `.keel/` into git.
/// Uses `.git/info/exclude` (repo-local, doesn't touch the tracked `.gitignore`).
fn exclude_keel(repo_root: &Path) {
    let excl = repo_root.join(".git/info/exclude");
    let cur = std::fs::read_to_string(&excl).unwrap_or_default();
    if cur.lines().any(|l| matches!(l.trim(), ".keel/" | ".keel")) {
        return;
    }
    let _ = std::fs::create_dir_all(repo_root.join(".git/info"));
    let mut s = cur;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(".keel/\n");
    let _ = std::fs::write(&excl, s);
}

/// Forward a command straight to the real `git` (identical behavior/stdio), then, for a mutating
/// verb in a keel-tracked repo, incrementally sync the new objects into the mirror. This is what
/// makes `keel <anything git understands>` a true drop-in. Never returns — it exits with git's code.
fn git_passthrough(args: &[String]) -> ! {
    use std::process::Command;
    let verb = args.first().cloned().unwrap_or_default();
    let status = match Command::new("git").args(args).status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("keel: could not run git: {e}");
            std::process::exit(127);
        }
    };
    if status.success() {
        sync_after(&verb); // best-effort; never fails the git command
    }
    std::process::exit(status.code().unwrap_or(if status.success() { 0 } else { 1 }));
}

/// After a mutating git command, fold any new objects into the keel mirror so the two stores stay
/// in step. Best-effort and quiet: a repo that isn't keel-tracked (no `.keel/store`) is skipped,
/// and a sync error warns but never masks git's own success.
fn sync_after(verb: &str) {
    const MUTATING: &[&str] = &[
        "commit", "merge", "pull", "fetch", "reset", "rebase", "revert", "cherry-pick", "am",
        "tag", "branch", "checkout", "switch", "stash", "apply", "restore", "rm", "mv", "add",
        "commit-tree", "update-ref", "push", "gc", "repack",
    ];
    if !MUTATING.contains(&verb) {
        return;
    }
    let top = match std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return, // not inside a git repo
    };
    let store_path = Path::new(&top).join(".keel/store");
    if !store_path.exists() {
        return; // repo isn't tracked by keel
    }
    let synced = keel_store::store::Store::open(&store_path)
        .map_err(|e| io::Error::other(e.to_string()))
        .and_then(|store| {
            ingest_repo(&store, &top, true)?;
            // Rebuild keel-native history for any new commits (incremental — only new ones), so
            // `keel brief` / provenance see work done through the git surface without a manual
            // `keel reindex`. Best-effort; a failure here never fails the git command.
            keel_git::bridge::bridge(&store)?;
            Ok(())
        });
    if let Err(e) = synced {
        eprintln!("keel: mirror sync skipped ({e})");
    }
}

/// `keel native <commit|status|log|diff>` — keel's own store-backed operations, kept reachable
/// now that the top-level verbs are git-compatible pass-throughs.
fn cmd_native(args: &[String]) -> io::Result<()> {
    match args.first().map(String::as_str) {
        Some("commit") => cmd_commit(&args[1..]),
        Some("status") => cmd_status(&args[1..]),
        Some("log") => cmd_log(&args[1..]),
        Some("diff") => cmd_diff(&args[1..]),
        _ => Err(io::Error::other("keel native: expected commit | status | log | diff")),
    }
}

/// `keel serve [--port N]` — serve this repo over git's smart-HTTP protocol, so a plain
/// `git clone http://host:port/<anything>` clones it *from keel* (no git binary on the server).
/// Currently serves fetch/clone (upload-pack); push (receive-pack) is next.
fn cmd_serve(args: &[String]) -> io::Result<()> {
    use std::net::TcpListener;
    let (_, store_path) = root_store(args)?;
    let port: u16 = flag(args, "--port").and_then(|s| s.parse().ok()).unwrap_or(8174);
    let store = keel_store::store::Store::open(&store_path).map_err(to_io)?;
    let refs = keel_git::server::advertised_refs(&store)?;
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("keel serve: git smart-HTTP on http://127.0.0.1:{port}/  ({} refs, store {})", refs.len(), store_path.display());
    println!("  try:  git clone http://127.0.0.1:{port}/repo");
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let store = store.clone(); // cheap: shares the LMDB env
        std::thread::spawn(move || {
            if let Err(e) = serve_conn(stream, &store) {
                eprintln!("keel serve: connection error: {e}");
            }
        });
    }
    Ok(())
}

/// Handle one HTTP request on `stream` and route it to the git smart-HTTP handlers.
fn serve_conn(mut stream: std::net::TcpStream, store: &keel_store::store::Store) -> io::Result<()> {
    use std::io::Read;
    // read headers (until CRLFCRLF)
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let headers_end = loop {
        if let Some(i) = find_sub(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 128 * 1024 {
            return respond(&mut stream, "431 Request Header Fields Too Large", "text/plain", b"");
        }
    };
    let head = String::from_utf8_lossy(&buf[..headers_end]).into_owned();
    let mut lines = head.split("\r\n");
    let req_line = lines.next().unwrap_or("");
    let mut rp = req_line.split(' ');
    let method = rp.next().unwrap_or("");
    let target = rp.next().unwrap_or("");
    let mut content_length = 0usize;
    let mut chunked = false;
    let mut gzip = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(": ") {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if k.eq_ignore_ascii_case("content-encoding") && v.to_ascii_lowercase().contains("gzip") {
                gzip = true;
            }
        }
    }
    if std::env::var("KEEL_SERVE_DEBUG").is_ok() {
        eprintln!("keel serve: {method} {target} content-length={content_length} chunked={chunked}");
    }
    // body: whatever came after the headers, plus whatever else the client sends. git uses
    // chunked transfer-encoding for the upload-pack POST, so decode that; otherwise Content-Length.
    let mut raw = buf[headers_end..].to_vec();
    let body = if chunked {
        loop {
            if let Some(decoded) = decode_chunked(&raw) {
                break decoded;
            }
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break decode_chunked(&raw).unwrap_or_default();
            }
            raw.extend_from_slice(&tmp[..n]);
        }
    } else {
        while raw.len() < content_length {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        raw.truncate(content_length);
        raw
    };
    // git gzip-compresses the upload-pack request body by default (Content-Encoding: gzip).
    let body = if gzip { gunzip(&body).unwrap_or(body) } else { body };

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method == "GET" && path.ends_with("/info/refs") {
        let service = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("service="))
            .unwrap_or("");
        if service == "git-upload-pack" || service == "git-receive-pack" {
            let adv = keel_git::smart_http::advertisement(store, service)?;
            let ct = format!("application/x-{service}-advertisement");
            return respond(&mut stream, "200 OK", &ct, &adv);
        }
        return respond(&mut stream, "403 Forbidden", "text/plain", b"unsupported service");
    }
    if method == "POST" && path.ends_with("/git-upload-pack") {
        let resp = keel_git::smart_http::upload_pack(store, &body)?;
        if std::env::var("KEEL_SERVE_DEBUG").is_ok() {
            let wants = body.windows(5).filter(|w| *w == b"want ").count();
            eprintln!("keel serve: upload-pack ~{wants} wants → {} byte response", resp.len());
        }
        return respond(&mut stream, "200 OK", "application/x-git-upload-pack-result", &resp);
    }
    if method == "POST" && path.ends_with("/git-receive-pack") {
        let resp = keel_git::smart_http::receive_pack(store, &body)?;
        // Rebuild keel-native history for the pushed commits so brief/provenance see them.
        let _ = keel_git::bridge::bridge(store);
        if std::env::var("KEEL_SERVE_DEBUG").is_ok() {
            eprintln!("keel serve: receive-pack {} byte request → {} byte report", body.len(), resp.len());
        }
        return respond(&mut stream, "200 OK", "application/x-git-receive-pack-result", &resp);
    }
    respond(&mut stream, "404 Not Found", "text/plain", b"not found")
}

fn respond(stream: &mut std::net::TcpStream, status: &str, content_type: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Decompress a gzip stream (git wraps the upload-pack request body in one). Parses the gzip
/// header (honoring the optional FEXTRA/FNAME/FCOMMENT/FHCRC fields), then raw-inflates the
/// DEFLATE payload (excluding the 8-byte CRC+size trailer). `None` if it isn't valid gzip.
fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 0x08 {
        return None;
    }
    let flg = data[3];
    let mut i = 10;
    if flg & 0x04 != 0 {
        // FEXTRA: 2-byte length + that many bytes
        let xlen = u16::from_le_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
        i += 2 + xlen;
    }
    if flg & 0x08 != 0 {
        // FNAME: NUL-terminated
        while *data.get(i)? != 0 {
            i += 1;
        }
        i += 1;
    }
    if flg & 0x10 != 0 {
        // FCOMMENT: NUL-terminated
        while *data.get(i)? != 0 {
            i += 1;
        }
        i += 1;
    }
    if flg & 0x02 != 0 {
        i += 2; // FHCRC
    }
    if i >= data.len() - 8 {
        return None;
    }
    miniz_oxide::inflate::decompress_to_vec(&data[i..data.len() - 8]).ok()
}

/// Decode an HTTP chunked-transfer body. Returns `Some(bytes)` once the terminating zero-length
/// chunk is present, or `None` if more input is still needed.
fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let nl = i + find_sub(&raw[i..], b"\r\n")?;
        let head = std::str::from_utf8(&raw[i..nl]).ok()?;
        let size = usize::from_str_radix(head.split(';').next()?.trim(), 16).ok()?;
        i = nl + 2;
        if size == 0 {
            return Some(out); // last chunk (trailers, if any, ignored)
        }
        if i + size + 2 > raw.len() {
            return None; // chunk body not fully arrived yet
        }
        out.extend_from_slice(&raw[i..i + size]);
        i += size + 2; // data + trailing CRLF
    }
}

/// `keel learn --lesson <text> [--task <text>]` — record the non-obvious thing this change taught,
/// attached to the current keel change (a post-hoc side-table annotation, so it works on git-driven
/// history). A later `keel brief` on this file or its neighbors surfaces the lesson — the flywheel.
fn cmd_learn(args: &[String]) -> io::Result<()> {
    let lesson = flag(args, "--lesson").ok_or_else(|| io::Error::other("usage: keel learn --lesson <text> [--task <text>]"))?;
    let task = flag(args, "--task").unwrap_or("");
    let (_root, store_path) = root_store(args)?;
    let repo = Repo::open(&store_path).map_err(to_io)?;
    let head = repo
        .head()
        .map_err(to_io)?
        .ok_or_else(|| io::Error::other("no keel history yet — commit first (e.g. `keel commit -m …`)"))?;
    repo.store().set_lesson(&head, task, lesson).map_err(to_io)?;
    println!("learned on {} — future briefs on this file or its neighbors will surface it", short(&head.to_hex()));
    Ok(())
}

/// `keel net-serve [--port N]` — serve this repo's objects + a coordination event channel over
/// QUIC, so remote agents can fetch objects and subscribe to fleet events (content-addressed,
/// verifiable). This is keel's multi-machine transport (NEW-1102).
fn cmd_net_serve(args: &[String]) -> io::Result<()> {
    let (_, store_path) = root_store(args)?;
    let port: u16 = flag(args, "--port").and_then(|s| s.parse().ok()).unwrap_or(9420);
    let store = keel_store::store::Store::open(&store_path).map_err(to_io)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let addr = format!("0.0.0.0:{port}").parse().map_err(|e| io::Error::other(format!("{e}")))?;
        let server = keel_net::Server::bind(addr, store).await.map_err(|e| io::Error::other(e.to_string()))?;
        println!("keel net-serve: QUIC on {} · objects + event channel (store {})", server.local_addr(), store_path.display());
        println!("  a peer fetches with keel_net::Client::connect(addr).get(oid); events via .subscribe()");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    })
}

/// `keel size` — logical content bytes held + object/chunk/delta counts.
fn cmd_size(args: &[String]) -> io::Result<()> {
    let (_root, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let s = repo.store();
    let bytes = s.stored_bytes().map_err(to_io)?;
    let objects = s.object_count().map_err(to_io)?;
    let chunks = s.chunk_count().map_err(to_io)?;
    let deltas = s.delta_count().map_err(to_io)?;
    if has(args, "--json") {
        println!(
            "{}",
            render_json(&json!({
                "stored_bytes": bytes, "objects": objects, "chunks": chunks, "deltas": deltas
            }))
        );
        return Ok(());
    }
    println!(
        "{bytes} bytes stored · {objects} objects ({deltas} in delta form) · {chunks} chunks"
    );
    Ok(())
}

/// `keel repack` — delta-compress history, then GC to reclaim the orphaned full forms.
/// Like `git gc`: safe to run any time, idempotent, never changes an address.
fn cmd_repack(args: &[String]) -> io::Result<()> {
    let (_root, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let stats = repo.repack().map_err(to_io)?;
    let gc = repo.store().gc().map_err(to_io)?;

    if has(args, "--json") {
        println!(
            "{}",
            render_json(&json!({
                "deltified": stats.deltified,
                "bytes_saved": stats.bytes_saved,
                "objects_reclaimed": gc.objects_removed,
                "objects_kept": gc.objects_kept,
            }))
        );
        return Ok(());
    }
    println!(
        "repacked {} blob version(s) as deltas ({} bytes saved); GC reclaimed {} orphaned object(s), {} kept",
        stats.deltified, stats.bytes_saved, gc.objects_removed, gc.objects_kept
    );
    Ok(())
}

fn cmd_diff(args: &[String]) -> io::Result<()> {
    let (root, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let head = repo.head().map_err(to_io)?;
    let only = flag(args, "--file");

    let changes: Vec<_> = repo
        .status(&root)
        .map_err(to_io)?
        .into_iter()
        .filter(|c| only.map(|f| c.path == f).unwrap_or(true))
        .collect();
    if changes.is_empty() {
        println!(
            "{}",
            if only.is_some() { "no changes to that path" } else { "working tree clean (matches HEAD)" }
        );
        return Ok(());
    }

    for c in &changes {
        // old = the file as of HEAD (absent → empty, e.g. an added file or no commits yet)
        let old = match head {
            Some(h) => repo.file_bytes_at(h, &c.path).map_err(to_io)?.unwrap_or_default(),
            None => Vec::new(),
        };
        // new = the working-tree file (a deleted file → empty)
        let new = match c.kind {
            ChangeKind::Deleted => Vec::new(),
            _ => std::fs::read(root.join(&c.path)).unwrap_or_default(),
        };
        print_file_diff(&c.path, c.kind, &old, &new);
    }
    Ok(())
}

/// Render one file's change as a plain unified diff. Binary content (NUL byte) is reported,
/// not dumped.
fn print_file_diff(path: &str, kind: ChangeKind, old: &[u8], new: &[u8]) {
    let tag = match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    };
    println!("── {path} ({tag})");
    if old.contains(&0) || new.contains(&0) {
        println!("   binary file differs ({} → {} bytes)", old.len(), new.len());
        return;
    }
    let (old, new) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
    for h in diff_lines(&old, &new) {
        println!("@@ -{},{} +{},{} @@", h.old_start, h.old_len, h.new_start, h.new_len);
        for line in h.lines {
            let mark = match line.tag {
                Tag::Context => ' ',
                Tag::Del => '-',
                Tag::Add => '+',
            };
            println!("{mark}{}", line.text);
        }
    }
}

/// `keel import <git-repo>` — ingest an existing git repo as NATIVE keel history (git is not
/// underneath; this reads git and replays first-parent commits as keel commits, preserving
/// author / message / timestamp). Bounded by `--limit` (most-recent N) to keep it affordable.
fn cmd_import(args: &[String]) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let src = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel import <git-repo> [--into <dir>] [--limit N]"))?;
    let into = flag(args, "--into").unwrap_or(src);
    let limit: usize = flag(args, "--limit").and_then(|s| s.parse().ok()).unwrap_or(200);
    let store = PathBuf::from(into).join(".keel/store");
    let repo = Repo::open(&store).map_err(to_io)?;

    // first-parent history, oldest→newest, unit-separator delimited
    let log = Command::new("git")
        .args(["-C", src, "log", "--first-parent", "--reverse", "--format=%H%x1f%an%x1f%at%x1f%s"])
        .output()?;
    if !log.status.success() {
        return Err(io::Error::other(format!("git log failed in {src} (not a git repo?)")));
    }
    let mut commits: Vec<(String, String, u64, String)> = String::from_utf8_lossy(&log.stdout)
        .lines()
        .filter_map(|l| {
            let mut p = l.split('\u{1f}');
            Some((
                p.next()?.to_string(),
                p.next()?.to_string(),
                p.next()?.parse().unwrap_or(0),
                p.next().unwrap_or("").to_string(),
            ))
        })
        .collect();
    let total = commits.len();
    if commits.len() > limit {
        commits = commits.split_off(total - limit); // keep the most recent `limit`
        eprintln!("keel import: {total} commits found; importing the most recent {limit} (raise with --limit)");
    }

    let base = std::env::temp_dir().join(format!("keel-import-{}", std::process::id()));
    let mut last = None;
    let mut n = 0usize;
    for (hash, author, ts, msg) in &commits {
        // materialize each commit into a FRESH, empty dir (git archive → tar). A reused dir
        // is wrong: tar extracts over stale files first-write-wins, so trees accumulate.
        let work = base.join(n.to_string());
        std::fs::create_dir_all(&work)?;
        let mut git =
            Command::new("git").args(["-C", src, "archive", hash]).stdout(Stdio::piped()).spawn()?;
        let git_out = git.stdout.take().expect("piped stdout");
        let tar = Command::new("tar").arg("-x").arg("-C").arg(&work).stdin(git_out).status()?;
        if !git.wait()?.success() || !tar.success() {
            return Err(io::Error::other(format!("could not materialize tree for {}", short(hash))));
        }
        let intent = if msg.is_empty() { "(no message)" } else { msg.as_str() };
        // uncached: distinct trees at identical paths in rapid succession would racily
        // false-hit the mtime+size stat cache.
        last = Some(repo.commit_dir_uncached(&work, intent, author, *ts, None).map_err(to_io)?);
        let _ = std::fs::remove_dir_all(&work); // one tree on disk at a time
        n += 1;
    }
    let _ = std::fs::remove_dir_all(&base);
    match last {
        Some(id) => println!("imported {n} commits into {} · HEAD {}", store.display(), short(&id.to_hex())),
        None => println!("nothing to import (no commits found)"),
    }
    Ok(())
}

/// `keel export <git-dir>` — mirror keel's first-parent history INTO a git repo (keel→git),
/// the removable coexistence bridge for human reviewers on GitHub during transition. keel
/// stays the system of record; this writes a git *copy*. Preserves author / message / date.
fn cmd_export(args: &[String]) -> io::Result<()> {
    use std::process::Command;
    let dst = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel export <git-dir> [--limit N]"))?;
    let limit: usize = flag(args, "--limit").and_then(|s| s.parse().ok()).unwrap_or(1000);
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;

    let mut hist = repo.log().map_err(to_io)?; // newest → oldest
    hist.reverse(); // oldest → newest
    if hist.len() > limit {
        let keep = hist.split_off(hist.len() - limit); // most-recent `limit`
        eprintln!("keel export: {} commits; exporting the most recent {limit}", hist.len() + keep.len());
        hist = keep;
    }

    let dstp = PathBuf::from(dst);
    std::fs::create_dir_all(&dstp)?;
    let git = |a: &[&str]| Command::new("git").arg("-C").arg(&dstp).args(a).status();
    git(&["init", "-q"])?;
    git(&["config", "user.email", "keel@local"])?;
    git(&["config", "user.name", "keel-export"])?;

    let work = std::env::temp_dir().join(format!("keel-export-{}", std::process::id()));
    let mut n = 0usize;
    for id in &hist {
        let c = repo.change(*id).map_err(to_io)?.ok_or_else(|| io::Error::other("missing change"))?;
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        repo.checkout_change(*id, &work).map_err(to_io)?; // materialize keel tree
        clear_worktree(&dstp)?; // drop tracked files (keep .git) so deletes propagate
        copy_dir(&work, &dstp)?;
        Command::new("git").arg("-C").arg(&dstp).args(["add", "-A"]).status()?;
        let author = format!("{} <keel@local>", c.author);
        let date = format!("@{}", c.timestamp);
        let intent = if c.intent.is_empty() { "(no message)".to_string() } else { c.intent.clone() };
        let ok = Command::new("git")
            .arg("-C").arg(&dstp)
            .args(["commit", "--allow-empty", "-q", "-m", &intent, "--author", &author, "--date", &date])
            .env("GIT_COMMITTER_DATE", &date)
            .status()?;
        if !ok.success() {
            return Err(io::Error::other(format!("git commit failed at keel change {}", short(&id.to_hex()))));
        }
        n += 1;
    }
    let _ = std::fs::remove_dir_all(&work);
    println!("exported {n} commits to {dst} (git mirror; keel remains the system of record)");
    Ok(())
}

/// Remove everything in `dir` except `.git` — so re-materializing a tree over it makes git
/// see removed files as deletions.
fn clear_worktree(dir: &Path) -> io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_name() == ".git" {
            continue;
        }
        let p = e.path();
        if e.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (from, to) = (e.path(), dst.join(e.file_name()));
        if e.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn cmd_verify(args: &[String]) -> io::Result<()> {
    let change = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel verify <change> --green|--red"))?;
    let v = if has(args, "--green") {
        Verification::Green
    } else if has(args, "--red") {
        Verification::Red
    } else {
        return Err(io::Error::other("specify --green or --red"));
    };
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let id = resolve_change(&repo, change)?; // accepts a full id or a short prefix (git-style)
    repo.store().set_verification(&id, v).map_err(to_io)?;
    let mark = if matches!(v, Verification::Green) { "green ✓" } else { "red ✗" };
    println!("verified {} · {mark}", short(&id.to_hex()));
    Ok(())
}

/// Resolve a change reference: a full 64-char hex id, or a short prefix matched (uniquely)
/// against the first-parent history — so `keel verify <8-char>` works like git.
fn resolve_change(repo: &Repo, s: &str) -> io::Result<ObjectId> {
    if let Some(id) = ObjectId::from_hex(s) {
        if repo.change(id).map_err(to_io)?.is_some() {
            return Ok(id);
        }
    }
    let hits: Vec<ObjectId> =
        repo.log().map_err(to_io)?.into_iter().filter(|id| id.to_hex().starts_with(s)).collect();
    match hits.len() {
        1 => Ok(hits[0]),
        0 => Err(io::Error::other(format!("no change matches '{s}'"))),
        _ => Err(io::Error::other(format!("ambiguous change prefix '{s}'"))),
    }
}

fn cmd_pin(args: &[String]) -> io::Result<()> {
    let symbol = args
        .first()
        .filter(|a| !a.starts_with('-') && !a.is_empty())
        .ok_or_else(|| io::Error::other("usage: keel pin <symbol> --lesson <text>"))?;
    let lesson = flag(args, "--lesson").ok_or_else(|| io::Error::other("--lesson is required"))?;
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    repo.store().set_pin(symbol, lesson).map_err(to_io)?;
    println!("pinned invariant on {symbol}");
    Ok(())
}

fn cmd_pins(args: &[String]) -> io::Result<()> {
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let pins = repo.store().pins().map_err(to_io)?;
    if pins.is_empty() {
        println!("(no pinned invariants)");
    }
    for (sym, lesson) in pins {
        println!("  {sym}: {lesson}");
    }
    Ok(())
}

fn cmd_sessions(args: &[String]) -> io::Result<()> {
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    // sessions attached to changes (optionally only those touching --file), newest first
    let changes: Vec<(keel_store::ObjectId, _)> = match flag(args, "--file") {
        Some(f) => repo.history_touching(f).map_err(to_io)?,
        None => {
            let mut v = Vec::new();
            for id in repo.log().map_err(to_io)? {
                if let Some(c) = repo.change(id).map_err(to_io)? {
                    v.push((id, c));
                }
            }
            v
        }
    };
    let mut shown = 0;
    for (id, c) in changes {
        let Some(sid) = c.session else { continue };
        if let Some(Object::Session(s)) = repo.store().get(&sid).map_err(to_io)? {
            let v = match repo.store().verification(&id).map_err(to_io).unwrap_or(Verification::Unverified) {
                Verification::Green => " ✓",
                Verification::Red => " ✗",
                _ => "",
            };
            let lesson = if s.lesson.is_empty() { String::new() } else { format!(" — {}", s.lesson) };
            println!("{} [{}] {}{}{}", short(&id.to_hex()), s.model, s.task, lesson, v);
            shown += 1;
        }
    }
    if shown == 0 {
        println!("(no sessions)");
    }
    Ok(())
}

fn cmd_session(args: &[String]) -> io::Result<()> {
    let change = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel session <change>"))?;
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let id = resolve_change(&repo, change)?;
    let c = repo.change(id).map_err(to_io)?.ok_or_else(|| io::Error::other("no such change"))?;
    let sid = c.session.ok_or_else(|| io::Error::other("that change has no session"))?;
    let Some(Object::Session(s)) = repo.store().get(&sid).map_err(to_io)? else {
        return Err(io::Error::other("session object missing"));
    };
    let count = |o: &Option<keel_store::ObjectId>| if o.is_some() { "yes" } else { "—" };
    println!("session for change {}", short(&id.to_hex()));
    println!("  task:      {}", s.task);
    println!("  model:     {}", s.model);
    if !s.lesson.is_empty() {
        println!("  lesson:    {}", s.lesson);
    }
    println!("  tokens:    in {} / out {}", s.tokens_in, s.tokens_out);
    println!("  verified:  {:?}", s.verification);
    println!("  prompts:   {}", count(&s.prompts));
    println!("  context:   {}", count(&s.context_served));
    println!("  tool_calls: {}   tool_results: {}", s.tool_calls.len(), s.tool_results.len());
    Ok(())
}

fn cmd_log(args: &[String]) -> io::Result<()> {
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let rows = match flag(args, "--file") {
        Some(f) => repo.history_touching(f).map_err(to_io)?,
        None => {
            let mut v = Vec::new();
            for id in repo.log().map_err(to_io)? {
                if let Some(c) = repo.change(id).map_err(to_io)? {
                    v.push((id, c));
                }
            }
            v
        }
    };
    if rows.is_empty() {
        println!("(no history)");
    }
    for (id, c) in rows {
        let sess = if c.session.is_some() { " [session]" } else { "" };
        println!("{} {} · {}{}", short(&id.to_hex()), c.intent, c.author, sess);
    }
    Ok(())
}

// ── daemon client ────────────────────────────────────────────────────────────

fn daemon_request(root: &Path, req: &Value) -> Option<Value> {
    let sock = root.join(".keel/daemon.sock");
    let mut stream = UnixStream::connect(&sock).ok()?;
    stream.write_all(serde_json::to_string(req).ok()?.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut reader = io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn to_io(e: StoreError) -> io::Error {
    io::Error::other(e.to_string())
}
fn root_store(args: &[String]) -> io::Result<(PathBuf, PathBuf)> {
    let root =
        flag(args, "--root").map(PathBuf::from).map(Ok).unwrap_or_else(std::env::current_dir)?;
    let store = flag(args, "--store").map(PathBuf::from).unwrap_or_else(|| root.join(".keel/store"));
    Ok((root, store))
}
fn default_sidecar_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../keel-resolve/sidecar"))
}

/// The resolver **sidecar directory** (holds `resolve.mjs` / `resolve-c.mjs`). `--resolver`
/// or `KEEL_RESOLVER` may name the dir, or a script file inside it (its parent is used, for
/// backward compatibility); otherwise the built-in sidecar dir.
fn sidecar_dir(args: &[String]) -> PathBuf {
    let raw = flag(args, "--resolver")
        .map(PathBuf::from)
        .or_else(|| std::env::var("KEEL_RESOLVER").ok().map(PathBuf::from));
    match raw {
        Some(p) if p.is_dir() => p,
        Some(p) => p.parent().map(Path::to_path_buf).unwrap_or(p),
        None => default_sidecar_dir(),
    }
}
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}
fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
fn short(hex: &str) -> &str {
    &hex[..8.min(hex.len())]
}
fn ok(v: bool) -> &'static str {
    if v {
        " ✓"
    } else {
        ""
    }
}
fn join_or_dash(v: &[String]) -> String {
    if v.is_empty() {
        "—".to_string()
    } else {
        v.join(", ")
    }
}

// ── rendering (both paths render from the JSON value) ────────────────────────

fn render_json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string())
}

fn render_human(v: &Value) -> String {
    use std::fmt::Write;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let arr = |k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let strs = |k: &str| arr(k).iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>();
    let field = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).unwrap_or("?").to_string();

    let mut out = String::new();
    let sym = v.get("symbol").and_then(Value::as_str).map(|x| format!("::{x}")).unwrap_or_default();
    let verdict = match s("verification") {
        "green" => "  ✓ green",
        "red" => "  ✗ red",
        _ => "",
    };
    let _ = writeln!(out, "brief · {} · {}{}{verdict}", s("task"), s("file"), sym);

    let ctx = arr("context");
    let tokens = v.get("tokens").and_then(Value::as_u64).unwrap_or(0);
    let trunc = v.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let _ = writeln!(
        out,
        "  context: {} defs, ~{} tokens{}",
        ctx.len(),
        tokens,
        if trunc { " (truncated to budget)" } else { "" }
    );
    for d in &ctx {
        let _ = writeln!(out, "    - {}::{}", field(d, "file"), field(d, "symbol"));
    }
    if let Some(err) = v.get("context_error").and_then(Value::as_str) {
        let _ = writeln!(out, "    (context unavailable: {err})");
    }
    let _ = writeln!(out, "  deps:  {}", join_or_dash(&strs("deps")));
    let _ = writeln!(out, "  rdeps: {}", join_or_dash(&strs("rdeps")));

    let inv = arr("invariants");
    if !inv.is_empty() {
        let _ = writeln!(out, "  ⚑ invariants (pinned):");
        for i in &inv {
            let _ = writeln!(out, "    - [{}] {}", field(i, "symbol"), field(i, "lesson"));
        }
    }

    let coord = arr("coordination");
    if !coord.is_empty() {
        let _ = writeln!(out, "  ⚠ coordination (held by others):");
        for c in &coord {
            let _ = writeln!(out, "    - {} held by {} ({})", field(c, "file"), field(c, "agent"), field(c, "task"));
        }
    }
    let pred = arr("predicted");
    if !pred.is_empty() {
        let _ = writeln!(out, "  ~ nearby (predicted collisions):");
        for p in &pred {
            let _ = writeln!(out, "    - {} by {} in {}/", field(p, "held_file"), field(p, "agent"), field(p, "dir"));
        }
    }
    let prov = arr("provenance");
    if !prov.is_empty() {
        let _ = writeln!(out, "  provenance:");
        for p in &prov {
            let ver = p.get("verified").and_then(Value::as_bool).unwrap_or(false);
            let _ = writeln!(out, "    - {} {} · {}{}", short(&field(p, "change")), field(p, "intent"), field(p, "author"), ok(ver));
        }
    }
    let sess = arr("sessions");
    if !sess.is_empty() {
        let _ = writeln!(out, "  prior sessions:");
        for x in &sess {
            let ver = x.get("verified").and_then(Value::as_bool).unwrap_or(false);
            let _ = writeln!(out, "    - [{}] {} — {}{}", short(&field(x, "change")), field(x, "task"), field(x, "lesson"), ok(ver));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_brief::{Brief, ContextDef, CoordConflict, Provenance, RelevantSession};

    fn sample() -> Value {
        Brief {
            task: "understand doA".into(),
            file: "a.ts".into(),
            symbol: Some("doA".into()),
            context: vec![ContextDef { file: "a.ts".into(), symbol: "doA".into(), text: "fn doA(){}".into() }],
            context_error: None,
            verification: Verification::Green,
            deps: vec!["b.ts".into()],
            rdeps: vec![],
            provenance: vec![Provenance {
                change: "abcdef1234567890".into(),
                intent: "init".into(),
                author: "acct:x".into(),
                verified: true,
            }],
            coordination: vec![CoordConflict { file: "a.ts".into(), agent: "bob".into(), task: "edit".into() }],
            predicted: vec![],
            sessions: vec![RelevantSession {
                change: "deadbeef00112233".into(),
                task: "earlier".into(),
                lesson: "settle before charge".into(),
                verified: true,
                has_context: true,
            }],
            invariants: vec![("doA".into(), "never call doA re-entrantly".into())],
            tokens: 42,
            truncated: false,
        }
        .to_json()
    }

    #[test]
    fn json_is_byte_stable_and_complete() {
        let v = sample();
        let a = render_json(&v);
        assert_eq!(a, render_json(&v), "JSON must be deterministic");
        for k in ["context", "deps", "rdeps", "provenance", "coordination", "sessions", "invariants", "tokens", "truncated"] {
            assert!(a.contains(&format!("\"{k}\"")), "missing key: {k}");
        }
        assert!(a.contains("settle before charge"));
        assert!(a.contains("never call doA re-entrantly"), "pinned invariant present");
    }

    #[test]
    fn human_render_has_key_sections() {
        let h = render_human(&sample());
        assert!(h.contains("a.ts::doA"));
        assert!(h.contains("deps:  b.ts"));
        assert!(h.contains("coordination"));
        assert!(h.contains("prior sessions"));
        assert!(h.contains("settle before charge"));
        assert!(h.contains("invariants"), "pinned invariants rendered");
    }
}
