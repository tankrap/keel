//! keel — the agent-facing CLI.
//!
//! `keel brief --file <f> [--symbol <s>] [--task <t>] [--budget N] [--json] [--reserve]`
//! runs the fused fetch: context (symbol slice) + graph (deps/rdeps) + coordination +
//! provenance + relevant prior sessions, in ONE call. If a `keeld` daemon is running for
//! the repo it answers (warm — no graph rebuild); otherwise the CLI runs it in-process.
//! `keel commit` / `keel log` are the write/history side.

use keel_brief::BriefService;
use keel_store::{
    diff_lines, semantic_summarize, ChangeGroup, ChangeKind, GroupKind, NodeKind, Object, ObjectId,
    Repo, Review, SemanticSummary, Session, StoreError, Tag, Verdict, Verification, Vfs,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Machine-readable failures when the caller asked for JSON — set before dispatch so the global
    // error path in `run` emits {ok:false, error, message, fix} instead of prose.
    JSON_MODE.store(has(&args, "--json"), std::sync::atomic::Ordering::Relaxed);
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
        Some("net-events") => run(cmd_net_events(&args[1..])),
        Some("verify") => run(cmd_verify(&args[1..])),
        Some("pin") => run(cmd_pin(&args[1..])),
        Some("pins") => run(cmd_pins(&args[1..])),
        Some("sessions") => run(cmd_sessions(&args[1..])),
        Some("session") => run(cmd_session(&args[1..])),
        Some("learn") => run(cmd_learn(&args[1..])),
        Some("review") => run(cmd_review(&args[1..])),
        Some("reviews") => run(cmd_reviews(&args[1..])),
        Some("walkthrough") => run(cmd_walkthrough(&args[1..])),
        Some("vfs") => run(cmd_vfs(&args[1..])),
        Some("why") => run(cmd_why(&args[1..])),
        Some("capture") => run(cmd_capture(&args[1..])),
        Some("erase") => run(cmd_erase(&args[1..])),
        Some("policy") => run(cmd_policy(&args[1..])),
        Some("native") => run(cmd_native(&args[1..])),
        // ── git-compatible surface ──
        Some("init") => run(cmd_init(&args[1..])),   // git init + keel store
        Some("clone") => run(cmd_clone(&args[1..])), // git clone + keel mirror
        Some("help") | None => print_usage(),
        // everything else IS a git command → forward to git (with mirror sync)
        Some(_) => git_passthrough(&args),
    }
}

/// Set once in `main` from `--json`, so the global error path in [`run`] can emit a machine-
/// readable failure (agents parse stdout as JSON on both success and error) instead of prose.
static JSON_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Build an error carrying an explicit fix hint, surfaced as the `fix` field under `--json` and
/// `(fix: …)` in prose. Encoded with a unit separator so it rides through `io::Error` unchanged.
fn fail_fix(message: impl std::fmt::Display, fix: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{message}\u{1f}{fix}"))
}

fn run(r: io::Result<()>) {
    if let Err(e) = r {
        let full = e.to_string();
        // An explicit fix (from `fail_fix`) rides after a unit separator; otherwise infer one.
        let (message, fix) = match full.split_once('\u{1f}') {
            Some((m, f)) => (m.to_string(), Some(f.to_string())),
            None => (full.clone(), fix_hint(&full)),
        };
        if JSON_MODE.load(std::sync::atomic::Ordering::Relaxed) {
            let mut o = json!({ "ok": false, "error": err_kind(&message), "message": message });
            if let Some(fix) = fix {
                o["fix"] = json!(fix);
            }
            println!("{}", render_json(&o));
        } else {
            eprint!("keel: {message}");
            if let Some(fix) = fix {
                eprint!("  (fix: {fix})");
            }
            eprintln!();
        }
        std::process::exit(1);
    }
}

/// A short, stable machine code for an error message — so an agent can branch on `error` without
/// parsing prose. Falls back to `"error"`.
fn err_kind(msg: &str) -> &'static str {
    let m = msg.to_lowercase();
    if m.contains("no keel history") || m.contains("commit first") {
        "no_history"
    } else if m.contains("not a git repo") || m.contains("not a keel repo") || m.contains("no .keel") {
        "not_a_repo"
    } else if m.starts_with("usage:") || m.contains("usage:") {
        "usage"
    } else if m.contains("no such") || m.contains("not found") || m.contains("does not exist") {
        "not_found"
    } else {
        "error"
    }
}

/// Infer a fix hint for common failures whose messages didn't carry one explicitly.
fn fix_hint(msg: &str) -> Option<String> {
    let m = msg.to_lowercase();
    if m.contains("no keel history") || m.contains("commit first") {
        Some("run `keel commit -m \"…\"` first".into())
    } else if m.contains("not a git repo") || m.contains("no .keel") || m.contains("not a keel repo") {
        Some("run `keel init` in the repository root".into())
    } else {
        msg.split_once("usage:").map(|rest| format!("usage:{}", rest.1))
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
         \x20 keel review --target <id> --verdict <approve|request-changes|reject|comment> [--label <l>]\n\
         \x20 keel reviews [--target <id>] [--label <l>] [--mentions <t>] [--no-human] [--disagreements] [--json]\n\
         \x20 keel walkthrough <change> [--full] [--semantic] [--json]   block-by-block (or operation-level) review\n\
         \x20 keel vfs <change> <ls|stat|cat> <path> [--json]   read a change's tree lazily, no checkout\n\
         \x20 keel repack [--json]                delta-compress history + GC (like `git gc`)\n\
         \x20 keel size   [--json]                logical bytes / object counts\n\
         \x20 keel mirror-in <git-repo>  ·  keel mirror-out <dir>  ·  keel import/export\n\
         \x20 keel verify <change> --green|--red\n\
         \x20 keel native <commit|status|log|diff>   keel's own store-backed operations\n\
         \x20 keel native diff --semantic            operation view: collapse bulk edits, surface anomalies\n\n\
         A running `keeld` for the repo answers briefs warm; otherwise runs in-process."
    );
}

fn cmd_init(args: &[String]) -> io::Result<()> {
    // git init first (drop-in), passing through any positional dir / git flags, then set up the
    // keel store alongside so the repo is both git- and keel-tracked from the start. keel's own
    // flags (--root/--store/--resolver, each with a value) are not git's — strip them, and when
    // --root names the target without a positional dir, hand git that root as the directory.
    let mut git_args: Vec<&String> = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--root" || a == "--store" || a == "--resolver" {
            skip = true;
            continue;
        }
        git_args.push(a);
    }
    let mut git = std::process::Command::new("git");
    git.arg("init").args(&git_args);
    if flag(args, "--root").is_some() && !git_args.iter().any(|a| !a.starts_with('-')) {
        git.arg(flag(args, "--root").unwrap());
    }
    let status = git.status();
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
    // Also record WHICH lessons this brief served (by the change each is attached to), so a
    // following `keel learn` can attribute the resulting work to them — the flywheel's feedback edge.
    let served: Vec<Value> = value
        .get("sessions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|s| s.get("change").cloned()).collect())
        .unwrap_or_default();
    let rec = json!({
        "task": value.get("task").cloned().unwrap_or(Value::Null),
        "file": value.get("file").cloned().unwrap_or(Value::Null),
        "context": value.get("context").cloned().unwrap_or(Value::Null),
        "served_lessons": served,
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
    // Secrets are scrubbed before content-addressing here too (NEW-1088), on by default; --no-scrub
    // opts out (with a warning) for a fully private/local store.
    let scrub = if has(args, "--no-scrub") {
        eprintln!("keel: warning — --no-scrub: storing the session unredacted; any secrets it contains will be permanent in the immutable store");
        false
    } else {
        true
    };
    let auto_ctx = last_brief_blob(&repo, &root);
    let session_id = if let Some(path) = flag(args, "--session") {
        let policy = CapturePolicy::load(&root);
        let ek = erase_key_of(args, author, &policy);
        let mut s = session_from_file(&repo, path, msg, flag(args, "--lesson"), scrub, ek)?;
        // Only auto-attach the served brief to a NON-erasable session: `last_brief_blob` is a plaintext
        // blob, so attaching it to an erasable commit would bolt on a non-erasable payload that `keel
        // erase` can't remove (the session file may still set its own context_served, which IS
        // encrypted). Skip it for erasable commits to keep the erasure guarantee honest.
        if ek.is_none() && s.context_served.is_none() {
            s.context_served = auto_ctx;
        }
        Some(repo.store().put(&Object::Session(s)).map_err(to_io)?)
    } else if let Some(lesson) = flag(args, "--lesson") {
        let s = Session {
            task: msg.to_string(),
            model: "keel".to_string(),
            lesson: if scrub { scrub_secrets(lesson).0 } else { lesson.to_string() },
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
fn session_from_file(repo: &Repo, path: &str, msg: &str, lesson_override: Option<&str>, scrub: bool, erase_key: Option<&str>) -> io::Result<Session> {
    let raw = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| io::Error::other(format!("session json: {e}")))?;
    session_from_value(repo, &v, msg, lesson_override, scrub, erase_key)
}

/// A per-org capture policy (repo `.keel/capture-policy.json`): the enterprise knobs from NEW-1088 —
/// what to store erasable by default, what to redact, and how much transcript to retain. All fields
/// default to today's behavior, so a repo with no policy file is unchanged.
#[derive(Clone)]
struct CapturePolicy {
    /// store captured payloads crypto-shreddable by default (as if `--erasable`).
    erasable: bool,
    /// the default key_id for erasable storage (else the author).
    erase_key: Option<String>,
    /// also redact PII (emails) — off by default (over-redaction would corrupt legitimate context).
    redact_pii: bool,
    /// retain the raw prompt transcript (a retention cap can drop it).
    retain_prompts: bool,
    /// retain tool outputs (a retention cap can drop them).
    retain_tool_results: bool,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        CapturePolicy { erasable: false, erase_key: None, redact_pii: false, retain_prompts: true, retain_tool_results: true }
    }
}

impl CapturePolicy {
    /// Load the repo's `.keel/capture-policy.json`, or the defaults if it's absent/unreadable/invalid.
    /// A PRESENT-but-invalid file warns loudly: silently falling back to defaults would disable an
    /// admin's redaction/erasability policy without a signal — a compliance footgun.
    fn load(root: &std::path::Path) -> CapturePolicy {
        let d = CapturePolicy::default();
        let path = root.join(".keel/capture-policy.json");
        let Ok(txt) = std::fs::read_to_string(&path) else { return d };
        let v: Value = match serde_json::from_str::<Value>(&txt) {
            Ok(v) if v.is_object() => v,
            _ => {
                eprintln!("keel: warning — {} is not valid JSON object; using the DEFAULT capture policy (redaction/erasability/retention NOT applied)", path.display());
                return d;
            }
        };
        let b = |k: &str, dflt: bool| v.get(k).and_then(Value::as_bool).unwrap_or(dflt);
        CapturePolicy {
            erasable: b("erasable", d.erasable),
            erase_key: v.get("erase_key").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            redact_pii: b("redact_pii", d.redact_pii),
            retain_prompts: b("retain_prompts", d.retain_prompts),
            retain_tool_results: b("retain_tool_results", d.retain_tool_results),
        }
    }
}

/// The repo root for a capture policy lookup: `--root` (or cwd) if it is a keel repo, else `None`.
fn policy_root(args: &[String]) -> Option<std::path::PathBuf> {
    let root = flag(args, "--root")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    root.join(".keel").is_dir().then_some(root)
}

/// Resolve the crypto-shred key_id for an erasable capture/commit. Erasable storage is enabled by
/// `--erasable`, an explicit `--erase-key <id>` (which also implies erasable), or the repo policy's
/// `erasable: true`. The key_id is `--erase-key` > policy `erase_key` > the author. `None` means store
/// payloads as plaintext blobs (still secret-scrubbed). A constant key_id = per-repo erase scope, a
/// unique one = per-session, the author = per-subject.
fn erase_key_of<'a>(args: &'a [String], author: &'a str, policy: &'a CapturePolicy) -> Option<&'a str> {
    if let Some(k) = flag(args, "--erase-key") {
        Some(k)
    } else if has(args, "--erasable") || policy.erasable {
        Some(policy.erase_key.as_deref().unwrap_or(author))
    } else {
        None
    }
}

/// `keel policy [--root <r>] [--json]` — show the effective per-org capture policy for this repo
/// (from `.keel/capture-policy.json`, else the defaults). Read-only; edit the JSON file to change it.
fn cmd_policy(args: &[String]) -> io::Result<()> {
    let root = policy_root(args).unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = root.join(".keel/capture-policy.json");
    // label the source honestly: a present-but-broken file reports as invalid, not as if it applied
    let source = if !path.is_file() {
        "defaults"
    } else if std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object).is_some() {
        "capture-policy.json"
    } else {
        "capture-policy.json (invalid → defaults)"
    };
    let p = CapturePolicy::load(&root);
    if has(args, "--json") {
        println!("{}", render_json(&json!({
            "source": source,
            "erasable": p.erasable, "erase_key": p.erase_key,
            "redact_pii": p.redact_pii, "retain_prompts": p.retain_prompts,
            "retain_tool_results": p.retain_tool_results,
        })));
    } else {
        println!("capture policy ({source})");
        println!("  erasable:            {}", p.erasable);
        println!("  erase_key:           {}", p.erase_key.as_deref().unwrap_or("(author)"));
        println!("  redact_pii:          {}", p.redact_pii);
        println!("  retain_prompts:      {}", p.retain_prompts);
        println!("  retain_tool_results: {}", p.retain_tool_results);
    }
    Ok(())
}

/// `keel erase <key-id> [--json]` — the right-to-erasure operation (NEW-1088). Crypto-shreds the data
/// key: every session payload captured under it (`keel capture --erasable`, default key = the author)
/// becomes permanently unrecoverable, while the immutable change graph — commits, trees, and the
/// task/lesson metadata — stays intact and verifiable. IRREVERSIBLE. Note it erases only THIS store;
/// backups/replicas retain the data until they are erased too.
fn cmd_erase(args: &[String]) -> io::Result<()> {
    let key_id = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel erase <key-id> --force (the --erase-key/author an erasable capture used; IRREVERSIBLE)"))?;
    // Irreversible: a mistyped key_id destroys the wrong subject's data with no undo. Require --force.
    if !has(args, "--force") {
        return Err(fail_fix(
            format!("`keel erase {key_id}` is IRREVERSIBLE — it permanently destroys every payload under this key"),
            "re-run with --force to confirm",
        ));
    }
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let existed = repo.store().has_key(key_id).map_err(to_io)?;
    let n = repo.store().shred_key(key_id).map_err(to_io)?;
    if has(args, "--json") {
        println!("{}", render_json(&json!({ "ok": true, "key_id": key_id, "erased": n, "key_existed": existed })));
    } else if existed {
        println!("erased '{key_id}' — {n} payload(s) now permanently unrecoverable (the change graph is untouched)");
    } else {
        println!("no key '{key_id}' — nothing to erase (already shredded, or no erasable capture used it)");
    }
    Ok(())
}

/// Build a [`Session`] from an already-parsed universal capture object (see [`session_from_file`]
/// for the shape). Shared by `commit --session` and the `capture` adapters.
fn session_from_value(repo: &Repo, v: &Value, msg: &str, lesson_override: Option<&str>, scrub: bool, erase_key: Option<&str>) -> io::Result<Session> {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    // The single choke point before any session text is content-addressed: scrub secrets here too
    // (unless the caller already scrubbed, or --no-scrub), so `keel commit --session <file>` — a
    // hand-supplied session file — gets the same protection as `keel capture` (NEW-1088).
    let scrub_txt = |t: &str| -> String { if scrub { scrub_secrets(t).0 } else { t.to_string() } };
    // When `erase_key` is set, the bulk payloads (prompts / context / tool calls & results) are
    // stored crypto-shreddable under that key_id instead of as plaintext blobs — so `keel erase
    // <key_id>` can later make them unrecoverable (right-to-erasure, NEW-1088). `task`/`lesson` stay
    // plaintext: they're the flywheel's retrieval metadata, not the sensitive transcript bulk.
    let blob = |text: &str| -> io::Result<ObjectId> {
        let t = scrub_txt(text);
        match erase_key {
            Some(kid) => repo.store().put_secret(t.as_bytes(), kid).map_err(to_io),
            None => repo.store().put(&Object::Blob(t.into_bytes())).map_err(to_io),
        }
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
        task: scrub_txt(s("task").unwrap_or(msg)),
        model: s("model").unwrap_or("unknown").to_string(),
        lesson: scrub_txt(lesson_override.or_else(|| s("lesson")).unwrap_or("")),
        prompts: opt_blob("prompts")?,
        context_served: opt_blob("context_served")?,
        tool_calls: blob_list("tool_calls")?,
        tool_results: blob_list("tool_results")?,
        verification,
        tokens_in: v.get("tokens_in").and_then(Value::as_u64).unwrap_or(0),
        tokens_out: v.get("tokens_out").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// `keel capture <session-log> [--tool auto|claude|aider] [--task T] [--lesson L] [-o file.json]
/// [--commit] [--json]` — the NEW-1088 adapters: turn a coding agent's native transcript (Claude
/// Code JSONL, Aider chat history) into keel's universal session capture, then either print/emit it
/// or commit it as a provenance change so `keel sessions` / `why` / `brief` treat it as first-class.
fn cmd_capture(args: &[String]) -> io::Result<()> {
    let log = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .ok_or_else(|| io::Error::other("usage: keel capture <session-log> [--tool claude|aider] [--commit] [--no-scrub] [--erasable [--erase-key <id>]] [--redact-pii] (Claude Code .jsonl / Aider history; secrets scrubbed unless --no-scrub; --erasable stores payloads crypto-shreddable so `keel erase` can remove them; --redact-pii also redacts emails; per-repo defaults via `keel policy`)"))?;
    let text = std::fs::read_to_string(&log)?;
    let tool = match flag(args, "--tool").unwrap_or("auto") {
        "auto" => detect_tool(&log, &text),
        t => t.to_string(),
    };
    let mut universal = match tool.as_str() {
        "claude" => capture_claude(&text),
        "aider" => capture_aider(&text),
        other => return Err(io::Error::other(format!("unknown --tool {other:?} (use claude or aider)"))),
    };
    // CLI overrides
    if let Some(t) = flag(args, "--task") {
        universal["task"] = json!(t);
    }
    if let Some(l) = flag(args, "--lesson") {
        universal["lesson"] = json!(l);
    }
    // Per-org capture policy (repo .keel/capture-policy.json): PII redaction, retention caps, default
    // erasability. Absent/unreadable → today's defaults, so nothing changes for a repo without one.
    let policy = policy_root(args).map(|r| CapturePolicy::load(&r)).unwrap_or_default();
    // Capture-time secret scrubbing BEFORE anything is written or content-addressed (NEW-1088). On by
    // default because a transcript/tool-output can carry live credentials and the store is immutable;
    // `--no-scrub` opts out for a fully private/local store (with a loud warning — it's a footgun).
    let redacted = if has(args, "--no-scrub") {
        eprintln!("keel: warning — --no-scrub: storing the raw transcript unredacted; any secrets it contains will be permanent in the immutable store");
        0
    } else {
        scrub_value(&mut universal)
    };
    // Opt-in PII (email) redaction, and retention caps that drop payloads entirely — both from policy
    // (or --redact-pii). Applied before the -o emit and before storage, so every downstream copy honours it.
    let pii_redacted = if has(args, "--redact-pii") || policy.redact_pii { scrub_value_pii(&mut universal) } else { 0 };
    if let Some(o) = universal.as_object_mut() {
        if !policy.retain_prompts {
            o.remove("prompts");
        }
        if !policy.retain_tool_results {
            o.remove("tool_results");
        }
    }
    // Use the SCRUBBED lesson (the override was written into `universal` before scrubbing), so a secret
    // in --lesson can't slip past into the session blob or the lesson record.
    let lesson = universal.get("lesson").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let task = universal.get("task").and_then(Value::as_str).unwrap_or("(captured session)").to_string();

    // Emit the universal JSON to a file if asked (feeds `keel commit --session <file>`).
    if let Some(out) = flag(args, "-o").or_else(|| flag(args, "--out")) {
        std::fs::write(out, render_json(&universal))?;
    }

    if has(args, "--commit") {
        let (root, store) = root_store(args)?;
        let repo = Repo::open(&store).map_err(to_io)?;
        // scrub=false: cmd_capture already scrubbed `universal` (and the lesson) above, respecting
        // --no-scrub — don't double-work.
        let author = flag(args, "--author").unwrap_or("capture");
        let erase_key = erase_key_of(args, author, &policy);
        let session = session_from_value(&repo, &universal, &task, lesson.as_deref(), false, erase_key)?;
        let sid = repo.store().put(&Object::Session(session)).map_err(to_io)?;
        let ts = now_secs();
        let cid = repo.commit_dir(&root, &task, author, ts, Some(sid)).map_err(to_io)?;
        if let Some(lesson) = lesson.as_deref() {
            repo.store().set_lesson(&cid, &task, lesson).map_err(to_io)?;
        }
        if has(args, "--json") {
            println!("{}", render_json(&json!({ "ok": true, "change": cid.to_hex(), "session": sid.to_hex(), "tool": tool, "redacted": redacted, "pii_redacted": pii_redacted })));
        } else {
            println!("captured {} session → change {} (session {}){}", tool, short(&cid.to_hex()), short(&sid.to_hex()), redaction_note(redacted, pii_redacted));
        }
        return Ok(());
    }

    // No --commit: report the parsed session (and/or the emitted file). Agents can pipe this on.
    let summary = json!({
        "ok": true, "tool": tool,
        "task": universal.get("task"), "model": universal.get("model"),
        "tokens_in": universal.get("tokens_in"), "tokens_out": universal.get("tokens_out"),
        "tool_calls": universal.get("tool_calls").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
        "redacted": redacted,
        "pii_redacted": pii_redacted,
    });
    if has(args, "--json") {
        println!("{}", render_json(&summary));
    } else {
        let note = redaction_note(redacted, pii_redacted);
        println!(
            "captured {} session: task={:?} model={} tools={}{} (add --commit to record it, or -o file.json)",
            tool,
            universal.get("task").and_then(Value::as_str).unwrap_or(""),
            universal.get("model").and_then(Value::as_str).unwrap_or("?"),
            universal.get("tool_calls").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
            note,
        );
    }
    Ok(())
}

/// Guess the source tool from the filename/content when `--tool auto`.
fn detect_tool(path: &str, text: &str) -> String {
    if path.ends_with(".jsonl") || text.contains("\"type\":\"assistant\"") || text.contains("\"type\": \"assistant\"") {
        "claude".into()
    } else if path.contains("aider") || path.ends_with(".md") {
        "aider".into()
    } else {
        "claude".into() // JSONL is the most common; parser tolerates non-matching lines
    }
}

/// Extract the visible text from a Claude message's `content` (a string, or blocks with `text`).
fn message_text(msg: &Value) -> Option<String> {
    match msg.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let joined: String = a
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

/// Claude Code transcript (`~/.claude/projects/**/<id>.jsonl`) → universal capture.
fn capture_claude(text: &str) -> Value {
    let (mut task, mut model) = (String::new(), String::new());
    let (mut tin, mut tout) = (0u64, 0u64);
    let mut tools: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
        let ty = d.get("type").and_then(Value::as_str).unwrap_or("");
        let msg = d.get("message");
        if task.is_empty() && ty == "user" {
            if let Some(t) = msg.and_then(message_text) {
                task = t;
            }
        }
        if let Some(m) = msg.and_then(|m| m.get("model")).and_then(Value::as_str) {
            if !m.is_empty() {
                model = m.to_string();
            }
        }
        if let Some(u) = msg.and_then(|m| m.get("usage")) {
            tin += u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
            tout += u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        }
        if let Some(arr) = msg.and_then(|m| m.get("content")).and_then(Value::as_array) {
            for b in arr {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let Some(n) = b.get("name").and_then(Value::as_str) {
                        if !tools.contains(&n.to_string()) {
                            tools.push(n.to_string()); // dedup: distinct tools used, not every call
                        }
                    }
                }
            }
        }
    }
    json!({
        "task": truncate_task(&task), "model": model,
        "tokens_in": tin, "tokens_out": tout, "tool_calls": tools, "prompts": text,
    })
}

/// Aider chat history (`.aider.chat.history.md`) → universal capture. User turns are `#### ` lines.
fn capture_aider(text: &str) -> Value {
    let task = text
        .lines()
        .find_map(|l| l.strip_prefix("#### ").map(str::trim))
        .or_else(|| text.lines().find(|l| !l.trim().is_empty()).map(str::trim))
        .unwrap_or("")
        .to_string();
    json!({ "task": truncate_task(&task), "model": "", "tokens_in": 0, "tokens_out": 0, "prompts": text })
}

/// Wall-clock seconds since the epoch (commit timestamp for a captured session).
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// First line of a task, bounded — a transcript's opening prompt can be long.
fn truncate_task(t: &str) -> String {
    let first = t.lines().next().unwrap_or("").trim();
    if first.chars().count() > 200 {
        format!("{}…", first.chars().take(199).collect::<String>())
    } else {
        first.to_string()
    }
}

// ── capture-time secret scrubbing (NEW-1088) ──────────────────────────────────────────────────
// A captured transcript + its tool OUTPUTS can carry live credentials, and once a session is
// content-addressed into the immutable store the bytes cannot be pulled back out. So we redact
// well-known secret shapes BEFORE anything is `put()`. High precision on purpose: agent transcripts
// are full of git/BLAKE3 hashes and `token = getToken()` code, so we match only distinctive,
// prefixed credentials + PEM blocks + JWTs — shapes that don't collide with hex hashes or
// identifiers. Un-prefixed high-entropy secrets (e.g. a bare AWS secret) and PII are a documented
// follow-on; over-redaction would corrupt the very code the flywheel retrieves.

/// A byte that can appear in a token body (the credential charset). `-`/`_` included so a full token
/// is consumed as one unit.
fn is_tok(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}

/// Length of the run of token bytes starting at `i`.
fn tok_run(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() && is_tok(b[j]) {
        j += 1;
    }
    j - i
}

/// Try to match a known credential shape at `b[i..]` (caller guarantees a token boundary before `i`).
/// Returns `(matched_len, placeholder)` on a hit. Each prefixed matcher consumes the whole trailing
/// token run and requires a minimum body length so short lookalikes (`sk-spinner`) don't match.
fn match_secret_at(b: &[u8], i: usize) -> Option<(usize, &'static str)> {
    let starts = |p: &[u8]| b[i..].starts_with(p);
    // (prefix, min body length after the prefix)
    const PREFIXED: &[(&[u8], usize)] = &[
        (b"sk-", 20),           // OpenAI / Anthropic (sk-ant-…, sk-proj-…) — all share sk-
        (b"ghp_", 36), (b"gho_", 36), (b"ghu_", 36), (b"ghs_", 36), (b"ghr_", 36), // GitHub tokens
        (b"github_pat_", 20),   // GitHub fine-grained PAT
        (b"glpat-", 20),        // GitLab PAT
        (b"AIza", 35),          // Google API key
        (b"xoxb-", 10), (b"xoxa-", 10), (b"xoxp-", 10), (b"xoxr-", 10), (b"xoxs-", 10), // Slack
    ];
    for (prefix, min_body) in PREFIXED {
        if starts(prefix) {
            let body_start = i + prefix.len();
            let body = tok_run(b, body_start);
            if body >= *min_body {
                // `sk-` is a weak prefix — a hyphenated slug like `sk-learn-model-checkpoint-v2` looks
                // the same. Real OpenAI/Anthropic keys have a high-entropy body (mixed case + digits),
                // so gate `sk-` on the body carrying BOTH an uppercase and a digit; lowercase slugs pass
                // through untouched.
                if *prefix == b"sk-" {
                    let seg = &b[body_start..body_start + body];
                    let entropic = seg.iter().any(u8::is_ascii_uppercase) && seg.iter().any(u8::is_ascii_digit);
                    if !entropic {
                        continue;
                    }
                }
                return Some((prefix.len() + body, "[REDACTED-SECRET]"));
            }
        }
    }
    // AWS access-key IDs: AKIA/ASIA + exactly 16 uppercase-alnum, ending at a boundary.
    for prefix in [b"AKIA".as_slice(), b"ASIA".as_slice()] {
        if starts(prefix) {
            let s = i + prefix.len();
            let run = tok_run(b, s);
            let all_upper = b[s..s + run].iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
            if run == 16 && all_upper {
                return Some((prefix.len() + 16, "[REDACTED-SECRET]"));
            }
        }
    }
    // JWT: three base64url segments (header starts `eyJ`) joined by dots.
    if starts(b"eyJ") {
        let s1 = tok_run(b, i);
        if s1 >= 12 && b.get(i + s1) == Some(&b'.') {
            let s2 = tok_run(b, i + s1 + 1);
            if s2 >= 8 && b.get(i + s1 + 1 + s2) == Some(&b'.') {
                let s3 = tok_run(b, i + s1 + 1 + s2 + 1);
                if s3 >= 8 {
                    return Some((s1 + 1 + s2 + 1 + s3, "[REDACTED-JWT]"));
                }
            }
        }
    }
    None
}

/// Redact PEM private-key blocks (`-----BEGIN … PRIVATE KEY-----` … `-----END … PRIVATE KEY-----`).
/// Returns the byte length of the whole block at `i` if one starts there.
fn match_pem_at(b: &[u8], i: usize) -> Option<usize> {
    const OPEN: &[u8] = b"-----BEGIN ";
    if !b[i..].starts_with(OPEN) {
        return None;
    }
    // the opening line must be a PRIVATE KEY header
    let line_end = b[i..].iter().position(|&c| c == b'\n').map(|p| i + p)?;
    if !b[i..line_end].windows(11).any(|w| w == b"PRIVATE KEY") {
        return None;
    }
    // find the matching END line and consume through its trailing dashes/newline. If the transcript
    // was TRUNCATED before the END line, redact the partial key — but BOUND the window (next blank
    // line, or a max block size) so a transcript that merely *mentions* a BEGIN header with no END
    // later doesn't swallow everything to EOF. A real key block has no blank line and fits the cap.
    let rest = &b[line_end..];
    let end_rel = match rest.windows(9).position(|w| w == b"-----END ") {
        Some(p) => p + line_end,
        None => {
            const MAX_PEM: usize = 8192; // > any real private key's base64
            let cap = (i + MAX_PEM).min(b.len());
            let end = b[line_end..cap]
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|p| line_end + p + 1)
                .unwrap_or(cap);
            return Some(end - i);
        }
    };
    let after = b[end_rel..].iter().position(|&c| c == b'\n').map(|p| end_rel + p + 1);
    // if the END line has no trailing newline, run to the end of the last dash run
    let end = after.unwrap_or_else(|| {
        let dashes = b[end_rel..].iter().position(|&c| c != b'-' && !c.is_ascii_alphanumeric() && c != b' ');
        dashes.map(|p| end_rel + p).unwrap_or(b.len())
    });
    Some(end - i)
}

/// Redact well-known secret shapes in `text`. Returns the scrubbed copy and the number of redactions.
/// Precise by design (see the module note above) — it will not touch hashes, identifiers, or prose.
fn scrub_secrets(text: &str) -> (String, usize) {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    let mut count = 0usize;
    while i < n {
        if let Some(len) = match_pem_at(b, i) {
            out.push_str("[REDACTED-PRIVATE-KEY]");
            i += len;
            count += 1;
            continue;
        }
        // token matchers require a boundary before `i` so we redact whole tokens, not word suffixes
        let boundary = i == 0 || !is_tok(b[i - 1]);
        if boundary {
            if let Some((len, placeholder)) = match_secret_at(b, i) {
                out.push_str(placeholder);
                i += len;
                count += 1;
                continue;
            }
        }
        // copy one UTF-8 scalar verbatim (matchers above are ASCII-only, so this is safe)
        let ch = text[i..].chars().next().unwrap();
        let clen = ch.len_utf8();
        out.push_str(&text[i..i + clen]);
        i += clen;
    }
    (out, count)
}

/// Recursively apply a text-scrub pass to every string in a captured-session JSON value, in place.
/// Returns the total number of redactions across all fields.
fn scrub_value_with(v: &mut Value, pass: &dyn Fn(&str) -> (String, usize)) -> usize {
    match v {
        Value::String(s) => {
            let (scrubbed, n) = pass(s);
            if n > 0 {
                *s = scrubbed;
            }
            n
        }
        Value::Array(a) => a.iter_mut().map(|x| scrub_value_with(x, pass)).sum(),
        Value::Object(o) => o.iter_mut().map(|(_, val)| scrub_value_with(val, pass)).sum(),
        _ => 0,
    }
}

/// Recursively scrub secrets from a captured-session JSON value (always-on). Returns the redaction
/// count so `keel capture` can report that scrubbing happened.
fn scrub_value(v: &mut Value) -> usize {
    scrub_value_with(v, &|t| scrub_secrets(t))
}

// ── opt-in PII redaction (per-org capture policy, NEW-1088) ────────────────────────────────────
// Emails are common, legitimate context (authors, config) AND sensitive PII, so — unlike secret
// scrubbing — this is OFF by default and enabled per-repo (capture-policy.json `redact_pii`) or with
// `--redact-pii`. Conservative: only well-formed `local@domain.tld` addresses, so an `@mention`, a
// `foo@bar` with no TLD, or a lone `@` in prose is left untouched.

fn is_email_local(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_domain_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'.' || c == b'-'
}

/// Length of an `local@domain.tld` email starting at `b[i]` (caller guarantees a boundary before `i`),
/// or `None`. Requires a non-empty local part, an `@`, and a domain ending in `.` + a 2+ alpha TLD.
fn match_email_at(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j < b.len() && is_email_local(b[j]) {
        j += 1;
    }
    if j == i || b.get(j) != Some(&b'@') {
        return None; // empty local part, or no '@'
    }
    let dom_start = j + 1;
    let mut k = dom_start;
    while k < b.len() && is_domain_char(b[k]) {
        k += 1;
    }
    // Trim trailing '.'/'-' — sentence punctuation like "email foo@bar.com." must not swallow the
    // final dot into the domain (which would empty the TLD and leak the whole address).
    while k > dom_start && matches!(b[k - 1], b'.' | b'-') {
        k -= 1;
    }
    let domain = &b[dom_start..k];
    let last_dot = domain.iter().rposition(|&c| c == b'.')?;
    if last_dot == 0 {
        return None; // domain starts with '.'
    }
    let tld = &domain[last_dot + 1..];
    if tld.len() < 2 || !tld.iter().all(u8::is_ascii_alphabetic) {
        return None;
    }
    Some(k - i)
}

/// Redact well-formed email addresses. Returns the scrubbed copy and the redaction count.
fn scrub_pii(text: &str) -> (String, usize) {
    let b = text.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    let mut count = 0usize;
    while i < b.len() {
        let boundary = i == 0 || !is_email_local(b[i - 1]);
        if boundary {
            if let Some(len) = match_email_at(b, i) {
                out.push_str("[REDACTED-EMAIL]");
                i += len;
                count += 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        let clen = ch.len_utf8();
        out.push_str(&text[i..i + clen]);
        i += clen;
    }
    (out, count)
}

/// Recursively redact PII (emails) from a captured-session JSON value. Opt-in (see the note above).
fn scrub_value_pii(v: &mut Value) -> usize {
    scrub_value_with(v, &|t| scrub_pii(t))
}

/// A human-readable ` · redacted N secret(s) + M email(s)` suffix for capture output (empty if none).
fn redaction_note(secrets: usize, pii: usize) -> String {
    let mut parts = Vec::new();
    if secrets > 0 {
        parts.push(format!("{secrets} secret(s)"));
    }
    if pii > 0 {
        parts.push(format!("{pii} email(s)"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" · redacted {}", parts.join(" + "))
    }
}

fn cmd_status(args: &[String]) -> io::Result<()> {
    let (root, store) = root_store(args)?;

    // Prefer the warm daemon: its fs-watched LiveStatus answers in O(changed) instead of walking
    // the whole tree. Fall back to a local walk if no daemon is up (or it can't serve status).
    let rows: Vec<Value> = match daemon_request(&root, &json!({ "op": "status" }))
        .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
        .and_then(|r| r.get("changes").and_then(Value::as_array).cloned())
    {
        Some(rows) => rows,
        None => {
            let repo = Repo::open(&store).map_err(to_io)?;
            repo.status(&root)
                .map_err(to_io)?
                .iter()
                .map(|c| json!({ "path": c.path, "status": c.kind.marker().to_string() }))
                .collect()
        }
    };

    if has(args, "--json") {
        println!("{}", render_json(&json!({ "changes": rows })));
        return Ok(());
    }

    if rows.is_empty() {
        println!("working tree clean (matches HEAD)");
        return Ok(());
    }
    let (mut a, mut m, mut d) = (0, 0, 0);
    for c in &rows {
        let marker = c.get("status").and_then(Value::as_str).unwrap_or("?");
        match marker {
            "A" => a += 1,
            "M" => m += 1,
            "D" => d += 1,
            _ => {}
        }
        println!("  {} {}", marker, c.get("path").and_then(Value::as_str).unwrap_or(""));
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
/// objects already in the mirror, then (re)build the session-retrieval index (sym + pat tags) over
/// that history. Run after commits made through the git surface so `keel brief` / `keel native log` /
/// provenance see them; also the backfill for a repo whose changes predate the retrieval index
/// (new keel-native commits self-index at commit time).
fn cmd_reindex(args: &[String]) -> io::Result<()> {
    let (_, store_path) = root_store(args)?;
    // One env for the whole command (opening the store twice in a process is unsafe): open the Repo
    // and thread its Store into the git-mirror steps, which take `&Store`.
    let repo = Repo::open(&store_path).map_err(to_io)?;
    let store = repo.store();
    // fold in any objects not yet mirrored (e.g. commits made via the git surface), then bridge
    if let Ok(top) = std::env::current_dir() {
        if top.join(".git").exists() {
            let _ = ingest_repo(store, top.to_str().unwrap_or("."), true);
        }
    }
    let b = keel_git::bridge::bridge(store)?;
    let tagged = keel_store::sessiontag::reindex_all(&repo).map_err(to_io)?;
    println!("keel reindexed: {} changes, {} trees, {} session-tag entries", b.commits, b.trees, tagged);
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
        Some("version") => cmd_version(),
        _ => Err(io::Error::other("keel native: expected commit | status | log | diff | version")),
    }
}

/// `keel native version` — the keel binary's OWN build identity (crate version + the git commit it
/// was built from, and whether that tree was dirty), as byte-stable JSON. Distinct from `keel
/// --version`, which proxies to the git binary keel wraps. Consumers pin/record this to tie a result
/// to the keel build that produced it. The git fields are embedded at build time by build.rs; an
/// empty commit means keel was built outside a checkout (or without git available). The stamp is
/// exact for a full build from a clean checkout (CI / the benchmark suite); on incremental rebuilds
/// `dirty` is best-effort (see build.rs).
fn cmd_version() -> io::Result<()> {
    println!("{}", render_json(&version_value()));
    Ok(())
}

/// The keel build identity as JSON. Pure (reads only compile-time env) so it's unit-testable.
fn version_value() -> Value {
    let commit = option_env!("KEEL_GIT_COMMIT").unwrap_or("");
    let dirty = option_env!("KEEL_GIT_DIRTY") == Some("1");
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git_commit": commit,
        "git_commit_short": commit.get(..12).unwrap_or(commit),
        "dirty": dirty,
    })
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

/// Ceiling on a request body `keel serve` will buffer. Without it a client can drive the process to
/// OOM by declaring a huge `Content-Length` (or streaming an unterminated chunked body) and feeding
/// bytes. Generous by default so a legitimate `git push` isn't rejected; override with
/// `KEEL_SERVE_MAX_BODY` (bytes) for larger packs.
fn max_body() -> usize {
    std::env::var("KEEL_SERVE_MAX_BODY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024)
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
    let cap = max_body();
    if content_length > cap {
        return respond(&mut stream, "413 Payload Too Large", "text/plain", b"");
    }
    let mut raw = buf[headers_end..].to_vec();
    let body = if chunked {
        loop {
            if let Some(decoded) = decode_chunked(&raw) {
                break decoded;
            }
            if raw.len() > cap {
                return respond(&mut stream, "413 Payload Too Large", "text/plain", b"");
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
        // Use checked arithmetic: `size` is an attacker-controlled hex value, so `i + size + 2`
        // can overflow, wrap past the bounds check, and then panic on an inverted slice range —
        // and with `panic = "abort"` that would take down the whole `keel serve` process.
        let end = i.checked_add(size)?;
        if end > raw.len() || end + 2 > raw.len() {
            return None; // chunk body (+ trailing CRLF) not fully arrived yet
        }
        out.extend_from_slice(&raw[i..end]);
        i = end + 2; // data + trailing CRLF
    }
}

/// `keel learn --lesson <text> [--task <text>]` — record the non-obvious thing this change taught,
/// attached to the current keel change (a post-hoc side-table annotation, so it works on git-driven
/// history). A later `keel brief` on this file or its neighbors surfaces the lesson — the flywheel.
/// `keel why <path> [--limit N] [--json]` — provenance for a file: the changes that touched it,
/// newest first, each with its intent, author, verification status, and (if the change carried an
/// agent session) the task and the lesson that session recorded. Answers "why is this code here,
/// what task produced it, and was it verified" — the agentic change model, queryable.
fn cmd_why(args: &[String]) -> io::Result<()> {
    let raw = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .ok_or_else(|| io::Error::other("usage: keel why <path> [--limit N] [--json]"))?;
    // Tolerate a `path:line` suffix; line-level blame is a follow-up, so resolve to the file.
    let path = raw.split(':').next().unwrap_or(&raw).to_string();
    let limit: usize = flag(args, "--limit").and_then(|s| s.parse().ok()).unwrap_or(10);
    let (_root, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;

    let mut recs: Vec<Value> = Vec::new();
    for (cid, c) in repo.history_touching_limited(&path, limit).map_err(to_io)?.iter() {
        // Authoritative verification is the side-table (set by `keel verify`), not the change's
        // commit-time field which stays Unverified.
        let verif = match repo.store().verification(cid).map_err(to_io)? {
            Verification::Green => "green",
            Verification::Red => "red",
            Verification::Unverified => "unverified",
        };
        let mut rec = json!({
            "change": cid.to_hex(), "verified": verif,
            "intent": c.intent, "author": c.author, "timestamp": c.timestamp,
        });
        // Lesson + task: prefer the recorded agent session, else the post-hoc `keel learn` lesson.
        if let Some(sid) = c.session {
            if let Some(Object::Session(s)) = repo.store().get(&sid).map_err(to_io)? {
                rec["task"] = json!(s.task);
                if !s.model.is_empty() {
                    rec["model"] = json!(s.model);
                }
                if !s.lesson.is_empty() {
                    rec["lesson"] = json!(s.lesson);
                }
            }
        }
        if rec.get("lesson").is_none() {
            if let Some((task, lesson)) = repo.store().lesson(cid).map_err(to_io)? {
                if !lesson.is_empty() {
                    rec["lesson"] = json!(lesson);
                    rec.as_object_mut().unwrap().entry("task").or_insert(json!(task));
                }
            }
        }
        recs.push(rec);
    }

    if has(args, "--json") {
        println!("{}", render_json(&json!({ "path": path, "history": recs })));
        return Ok(());
    }
    if recs.is_empty() {
        println!("no recorded history for {path} (never committed, or outside keel history)");
        return Ok(());
    }
    println!("why {path} — {} change(s), newest first:", recs.len());
    for r in &recs {
        let v = r["verified"].as_str().unwrap_or("");
        let mark = match v {
            "green" => "✓",
            "red" => "✗",
            _ => "·",
        };
        println!(
            "  {mark} {} [{v}]  {}",
            short(r["change"].as_str().unwrap_or("")),
            r["intent"].as_str().unwrap_or("")
        );
        println!(
            "      by {} · {}",
            r["author"].as_str().unwrap_or(""),
            rel_time(r["timestamp"].as_u64().unwrap_or(0))
        );
        if let Some(task) = r.get("task").and_then(Value::as_str) {
            println!("      task: {task}");
        }
        if let Some(lesson) = r.get("lesson").and_then(Value::as_str) {
            println!("      lesson: {lesson}");
        }
    }
    Ok(())
}

/// Coarse relative time for human output (`3d ago`). 0 → unknown.
fn rel_time(ts: u64) -> String {
    if ts == 0 {
        return "unknown time".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(ts);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// keel-global flags that take a value; their argument must not be mistaken for a positional.
const VALUE_FLAGS: [&str; 3] = ["--root", "--store", "--resolver"];

/// The first positional argument (not a flag, and not the value consumed by a value-flag). So
/// `keel walkthrough --root DIR <change>` resolves `<change>`, not `DIR` — a plain "first non-dash
/// token" scan would wrongly pick `DIR`.
fn first_positional(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            i += 2; // skip the flag and its value
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(a);
    }
    None
}

/// All positional arguments in order, skipping flags and the values consumed by value-flags — the
/// multi-positional companion to [`first_positional`], so `keel vfs --root DIR <change> ls <path>`
/// yields `[<change>, ls, <path>]`, not `[DIR, <change>, ls, <path>]`.
fn positionals(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        out.push(a);
        i += 1;
    }
    out
}

/// Every value that follows a repeated flag (`--label a --label b` → ["a","b"]).
fn flag_all(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `keel review --target <id> --verdict <approve|request-changes|reject|comment> [--reviewer <who>]
/// [--human] [--summary <text>] [--label <l> ...] [--finding <blob-id> ...]`
///
/// Record a review of a session or change as a first-class object (peer to a session). Several
/// reviews of one target with different verdicts express disagreement.
fn cmd_review(args: &[String]) -> io::Result<()> {
    let target = flag(args, "--target").and_then(ObjectId::from_hex).ok_or_else(|| {
        fail_fix("missing or invalid --target", "pass the session/change id as 64 hex chars")
    })?;
    let verdict = Verdict::from_name(flag(args, "--verdict").unwrap_or("comment"))
        .ok_or_else(|| fail_fix("invalid --verdict", "one of: approve, request-changes, reject, comment"))?;
    let reviewer = flag(args, "--reviewer").unwrap_or("agent").to_string();
    let by_human = has(args, "--human");
    let summary = flag(args, "--summary").unwrap_or("").to_string();
    let labels = flag_all(args, "--label");
    let findings: Vec<ObjectId> =
        flag_all(args, "--finding").iter().filter_map(|s| ObjectId::from_hex(s)).collect();

    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let target_known = repo.store().has(&target).map_err(to_io)?;
    let review = Review { target, reviewer, by_human, verdict, summary, labels, findings };
    let id = repo.store().record_review(&review).map_err(to_io)?;
    println!(
        "{}",
        render_json(&json!({
            "ok": true, "review": id.to_hex(), "target": target.to_hex(),
            "verdict": verdict.name(), "by_human": by_human, "target_known": target_known,
        }))
    );
    Ok(())
}

/// `keel reviews [--target <id>] [--label <l>] [--mentions <text>] [--verdict <v>] [--no-human]
/// [--disagreements] [--json]`
///
/// Query recorded reviews. Because reviews are first-class objects, questions git can't express
/// become filters here: reviews mentioning a topic, security-critical reviews, changes approved
/// without a human, and targets where reviewers disagreed.
fn cmd_reviews(args: &[String]) -> io::Result<()> {
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let mut reviews = repo.store().reviews().map_err(to_io)?;

    if let Some(t) = flag(args, "--target").and_then(ObjectId::from_hex) {
        reviews.retain(|(_, r)| r.target == t);
    }
    if let Some(l) = flag(args, "--label") {
        reviews.retain(|(_, r)| r.labels.iter().any(|x| x == l));
    }
    if let Some(m) = flag(args, "--mentions") {
        let m = m.to_lowercase();
        reviews.retain(|(_, r)| r.summary.to_lowercase().contains(&m));
    }
    if let Some(v) = flag(args, "--verdict").and_then(Verdict::from_name) {
        reviews.retain(|(_, r)| r.verdict == v);
    }
    if has(args, "--no-human") {
        reviews.retain(|(_, r)| !r.by_human);
    }
    if has(args, "--disagreements") {
        // keep only reviews whose target carries more than one distinct verdict
        use std::collections::{HashMap, HashSet};
        let mut by_target: HashMap<[u8; 32], HashSet<u8>> = HashMap::new();
        for (_, r) in repo.store().reviews().map_err(to_io)? {
            by_target.entry(r.target.0).or_default().insert(r.verdict.tag());
        }
        reviews.retain(|(_, r)| by_target.get(&r.target.0).is_some_and(|s| s.len() >= 2));
    }
    // stable output order: by target, then verdict
    reviews.sort_by(|a, b| {
        a.1.target.0.cmp(&b.1.target.0).then(a.1.verdict.tag().cmp(&b.1.verdict.tag()))
    });

    if has(args, "--json") {
        let arr: Vec<Value> = reviews
            .iter()
            .map(|(id, r)| {
                json!({
                    "id": id.to_hex(), "target": r.target.to_hex(), "reviewer": r.reviewer,
                    "by_human": r.by_human, "verdict": r.verdict.name(),
                    "summary": r.summary, "labels": r.labels,
                })
            })
            .collect();
        println!("{}", render_json(&json!({ "ok": true, "count": arr.len(), "reviews": arr })));
    } else if reviews.is_empty() {
        println!("(no reviews)");
    } else {
        for (id, r) in &reviews {
            let h = if r.by_human { " human" } else { "" };
            let labels =
                if r.labels.is_empty() { String::new() } else { format!(" [{}]", r.labels.join(",")) };
            println!(
                "{} {} {}{}{} — {}",
                short(&id.to_hex()),
                short(&r.target.to_hex()),
                r.verdict.name(),
                h,
                labels,
                r.summary
            );
        }
    }
    Ok(())
}

/// `keel walkthrough <change> [--json] [--full]`
///
/// The reviewer walkthrough: instead of handing a reviewer a raw diff to read line by line, keel
/// narrates the change block by block from what it already recorded — the session's task and model,
/// the change's verification verdict, and, for each file, the proof that backs it. "Proof" is
/// concrete, not prose: a test file is its own evidence; a source file is linked to the test files
/// that changed alongside it; anything else leans on the recorded green/red verdict, and an untested
/// block that never went green is flagged for a closer look.
///
/// git has no session or verdict to narrate from, so its review is always "read every line and hope
/// you catch the one that matters." keel derives the walkthrough from the recorded work, which is why
/// it's a lookup here and a re-generation nowhere.
fn cmd_walkthrough(args: &[String]) -> io::Result<()> {
    let change = first_positional(args)
        .ok_or_else(|| io::Error::other("usage: keel walkthrough <change> [--json] [--full]"))?;
    let full = has(args, "--full");
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let id = resolve_change(&repo, change)?;
    let c = repo.change(id).map_err(to_io)?.ok_or_else(|| io::Error::other("no such change"))?;

    // Authoritative verdict comes from the side-table (`keel verify`), not the change's commit-time
    // field, which stays Unverified after the fact.
    let verif = match repo.store().verification(&id).map_err(to_io)? {
        Verification::Green => "green",
        Verification::Red => "red",
        Verification::Unverified => "unverified",
    };

    // Session narrative (optional): the task the agent was given, the model, the lesson it learned.
    let (mut task, mut model, mut lesson) = (None, None, None);
    if let Some(sid) = c.session {
        if let Some(Object::Session(s)) = repo.store().get(&sid).map_err(to_io)? {
            if !s.task.is_empty() {
                task = Some(s.task);
            }
            if !s.model.is_empty() {
                model = Some(s.model);
            }
            if !s.lesson.is_empty() {
                lesson = Some(s.lesson);
            }
        }
    }

    let files = repo.change_files(id).map_err(to_io)?;
    let parent = c.parents.first().copied();

    // `--semantic`: the operation view of this change under the session header — bulk edits collapsed,
    // unique changes and smuggled-constant anomalies surfaced — instead of block-by-block hunks.
    if has(args, "--semantic") {
        return walkthrough_semantic(
            &repo, id, &c.intent, &c.author, c.timestamp, verif,
            task.as_deref(), model.as_deref(), lesson.as_deref(), &files, parent, has(args, "--json"),
        );
    }

    // The test files this change touched — the pool each source block draws its proof link from.
    let test_paths: Vec<String> =
        files.iter().filter(|f| is_test_path(&f.path)).map(|f| f.path.clone()).collect();

    let mut blocks = Vec::new();
    let (mut tot_add, mut tot_del) = (0usize, 0usize);
    for f in &files {
        let old = match (parent, f.kind) {
            (_, ChangeKind::Added) => Vec::new(),
            (Some(p), _) => repo.file_bytes_at(p, &f.path).map_err(to_io)?.unwrap_or_default(),
            (None, _) => Vec::new(),
        };
        let new = match f.kind {
            ChangeKind::Deleted => Vec::new(),
            _ => repo.file_bytes_at(id, &f.path).map_err(to_io)?.unwrap_or_default(),
        };
        let block = walkthrough_block(&f.path, f.kind, &old, &new, &test_paths, verif, full);
        tot_add += block["added_lines"].as_u64().unwrap_or(0) as usize;
        tot_del += block["deleted_lines"].as_u64().unwrap_or(0) as usize;
        blocks.push(block);
    }
    let covered = blocks.iter().filter(|b| b["proof"]["status"] == "covered").count();
    let untested = blocks.iter().filter(|b| b["proof"]["status"] == "untested").count();

    let mut out = json!({
        "ok": true,
        "change": id.to_hex(),
        "intent": c.intent,
        "author": c.author,
        "timestamp": c.timestamp,
        "verification": verif,
        "files": files.len(),
        "added_lines": tot_add,
        "deleted_lines": tot_del,
        "blocks": blocks,
        "proof_summary": {
            "verified": verif == "green",
            "covered": covered,
            "untested": untested,
            "tests_changed": test_paths.len(),
        },
    });
    if let Some(t) = task {
        out["task"] = json!(t);
    }
    if let Some(m) = model {
        out["model"] = json!(m);
    }
    if let Some(l) = lesson {
        out["lesson"] = json!(l);
    }

    if has(args, "--json") {
        println!("{}", render_json(&out));
    } else {
        print!("{}", render_walkthrough_human(&out));
    }
    Ok(())
}

/// Does this path look like a test/spec file? Covers the conventions in play across the benchmark
/// corpora: a `test`/`tests`/`spec`/`__tests__` path segment (django, tokio), a `_test`/`_spec`
/// basename suffix (prometheus/go), a `test_` prefix (django/py), or a `.test.`/`.spec.` infix
/// (vs code/ts). Deliberately a heuristic — its only job is to pick which proof link to surface.
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    if lower.split('/').any(|p| matches!(p, "test" | "tests" | "spec" | "__tests__")) {
        return true;
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if base.contains(".test.") || base.contains(".spec.") {
        return true;
    }
    let stem = base.split('.').next().unwrap_or(base);
    stem.starts_with("test_")
        || stem.starts_with("spec_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
}

/// Does `test_path` plausibly exercise `src_path`? True when, after stripping the usual test affixes,
/// the test's basename stem equals the source's basename stem (`core.rs` ↔ `core_test.rs`,
/// `parser.ts` ↔ `parser.test.ts`, `views.py` ↔ `test_views.py`). Same stem in a different directory
/// still counts — the link is a reviewer hint, so a generous match is the right bias.
fn test_covers(src_path: &str, test_path: &str) -> bool {
    fn stem(p: &str) -> String {
        let base = p.rsplit('/').next().unwrap_or(p).to_lowercase();
        base.split('.').next().unwrap_or(&base).to_string()
    }
    let src = stem(src_path);
    if src.is_empty() {
        return false;
    }
    let t = stem(test_path);
    // The test's stem is the source stem plus ONE anchored affix. Each candidate affix is tried
    // independently against a fresh copy of `t` — never chained — so `spec_test` (a `_test` suffix on
    // `spec`) is recognized as covering `spec`, not misread as a `spec_` prefix on `test` that then
    // gets its bare `test` chewed off to nothing. Affixes are underscore-anchored on purpose: a bare
    // `test`/`spec` strip would maul names like `latest` and empty out `spec_test`.
    t == src
        || t.strip_suffix("_test").is_some_and(|r| r == src)
        || t.strip_suffix("_spec").is_some_and(|r| r == src)
        || t.strip_prefix("test_").is_some_and(|r| r == src)
        || t.strip_prefix("spec_").is_some_and(|r| r == src)
}

/// One file's block in the walkthrough: its diff shape (added/deleted line counts, hunk headers, and
/// — with `--full` — hunk bodies) plus its proof link. A binary file is reported, not dumped.
fn walkthrough_block(
    path: &str,
    kind: ChangeKind,
    old: &[u8],
    new: &[u8],
    test_paths: &[String],
    verif: &str,
    full: bool,
) -> Value {
    let kind_s = match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    };
    let binary = old.contains(&0) || new.contains(&0);
    let (mut add, mut del) = (0usize, 0usize);
    let mut hunk_heads = Vec::new();
    let mut hunk_bodies = Vec::new();
    if !binary {
        let (o, n) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
        for h in diff_lines(&o, &n) {
            hunk_heads.push(format!("@@ -{},{} +{},{} @@", h.old_start, h.old_len, h.new_start, h.new_len));
            let mut body = Vec::new();
            for line in &h.lines {
                match line.tag {
                    Tag::Add => add += 1,
                    Tag::Del => del += 1,
                    Tag::Context => {}
                }
                if full {
                    let m = match line.tag {
                        Tag::Context => ' ',
                        Tag::Del => '-',
                        Tag::Add => '+',
                    };
                    body.push(format!("{m}{}", line.text));
                }
            }
            if full {
                hunk_bodies.push(body);
            }
        }
    }
    // Proof link: a test file is its own evidence; a source file points at the tests that changed
    // with it; otherwise the block leans on the recorded verdict, and an untested non-green block
    // is the one a reviewer should actually open.
    let is_test = is_test_path(path);
    let covering: Vec<String> = if is_test {
        Vec::new()
    } else {
        test_paths.iter().filter(|t| test_covers(path, t)).cloned().collect()
    };
    let status = if is_test {
        "is-test"
    } else if !covering.is_empty() {
        "covered"
    } else if verif == "green" {
        "verified"
    } else {
        "untested"
    };
    let mut b = json!({
        "path": path,
        "kind": kind_s,
        "binary": binary,
        "added_lines": add,
        "deleted_lines": del,
        "hunks": hunk_heads,
        "proof": { "status": status, "tests": covering, "verification": verif },
    });
    if full && !hunk_bodies.is_empty() {
        b["hunk_bodies"] = json!(hunk_bodies);
    }
    b
}

/// Render the walkthrough for a human reviewer: a header (verdict, author, task, scope, proof tally),
/// then one narrated block per file — each opening with its proof link so the reviewer reads the
/// evidence before the diff — and a closing nudge to record a verdict as a first-class review object.
fn render_walkthrough_human(v: &Value) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let verif = v["verification"].as_str().unwrap_or("");
    let mark = match verif {
        "green" => "✓",
        "red" => "✗",
        _ => "·",
    };
    let change = v["change"].as_str().unwrap_or("");
    let _ = writeln!(s, "walkthrough · {} {}  {}", mark, short(change), v["intent"].as_str().unwrap_or(""));
    let _ = writeln!(
        s,
        "  by {} · {} · verified {verif}",
        v["author"].as_str().unwrap_or(""),
        rel_time(v["timestamp"].as_u64().unwrap_or(0))
    );
    if let Some(t) = v.get("task").and_then(Value::as_str) {
        let _ = writeln!(s, "  task:  {t}");
    }
    if let Some(m) = v.get("model").and_then(Value::as_str) {
        let _ = writeln!(s, "  model: {m}");
    }
    if let Some(l) = v.get("lesson").and_then(Value::as_str) {
        let _ = writeln!(s, "  lesson: {l}");
    }
    let _ = writeln!(s, "  scope: {} file(s), +{} −{}", v["files"], v["added_lines"], v["deleted_lines"]);
    let ps = &v["proof_summary"];
    let _ = writeln!(
        s,
        "  proof: {} covered · {} untested · {} test file(s) changed",
        ps["covered"], ps["untested"], ps["tests_changed"]
    );
    let _ = writeln!(s);
    if let Some(blocks) = v["blocks"].as_array() {
        for (i, b) in blocks.iter().enumerate() {
            let _ = writeln!(
                s,
                "  {}. {} ({}) +{} −{}",
                i + 1,
                b["path"].as_str().unwrap_or(""),
                b["kind"].as_str().unwrap_or(""),
                b["added_lines"],
                b["deleted_lines"]
            );
            let proof_txt = match b["proof"]["status"].as_str().unwrap_or("") {
                "is-test" => "proof: this file IS the test evidence for the change".to_string(),
                "covered" => {
                    let tests: Vec<&str> = b["proof"]["tests"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    format!("proof: exercised by {}", tests.join(", "))
                }
                "verified" => "proof: change verified green (no test changed alongside)".to_string(),
                _ => "proof: UNTESTED — no colocated test and change not green; review closely".to_string(),
            };
            let _ = writeln!(s, "       {proof_txt}");
            let heads = b["hunks"].as_array().cloned().unwrap_or_default();
            if b["binary"].as_bool().unwrap_or(false) {
                let _ = writeln!(s, "       (binary file)");
            } else if let Some(bodies) = b.get("hunk_bodies").and_then(Value::as_array) {
                // --full: print each hunk's header immediately followed by its own body, so multi-hunk
                // files stay readable (a reviewer can map every body line back to its `@@` range).
                for (j, body) in bodies.iter().enumerate() {
                    if let Some(h) = heads.get(j).and_then(Value::as_str) {
                        let _ = writeln!(s, "       {h}");
                    }
                    for l in body.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        let _ = writeln!(s, "         {l}");
                    }
                }
            } else {
                for h in heads.iter().filter_map(Value::as_str) {
                    let _ = writeln!(s, "       {h}");
                }
            }
            let _ = writeln!(s);
        }
    }
    let _ = writeln!(
        s,
        "  → record a verdict: keel review --target {change} --verdict <approve|request-changes|reject> [--human]"
    );
    s
}

/// `keel vfs <change> <ls|stat|cat> <path> [--json]`
///
/// Read a change's tree as a lazy filesystem, straight from the content-addressed store — no
/// checkout. This is the read core behind a future `keel mount`: opening the view touches only the
/// root tree, and each op fetches just the trees and blobs on the path it needs, so it behaves the
/// same whether the change has 200 files or a million. Every invocation reports what it actually
/// fetched (`fetched N tree(s), M blob(s), K byte(s)`) — the evidence that access is lazy rather than
/// a whole-repo read.
fn cmd_vfs(args: &[String]) -> io::Result<()> {
    let pos = positionals(args);
    let change = pos
        .first()
        .copied()
        .ok_or_else(|| io::Error::other("usage: keel vfs <change> <ls|stat|cat> <path> [--json]"))?;
    let op = pos.get(1).copied().unwrap_or("ls");
    let path = pos.get(2).copied().unwrap_or("");
    let json = has(args, "--json");

    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;
    let id = resolve_change(&repo, change)?;
    let c = repo.change(id).map_err(to_io)?.ok_or_else(|| io::Error::other("no such change"))?;
    let vfs = Vfs::new(repo.store(), c.tree);

    // Emit the fetch tally: on stderr in prose (so piped stdout stays clean), inline under --json.
    let report = |vfs: &Vfs| {
        let s = vfs.stats();
        if !json {
            eprintln!(
                "fetched {} tree(s), {} blob(s), {} byte(s)",
                s.trees_read, s.blobs_read, s.bytes_read
            );
        }
        json!({ "trees": s.trees_read, "blobs": s.blobs_read, "bytes": s.bytes_read })
    };

    match op {
        "ls" => {
            let entries = vfs
                .readdir(path)
                .map_err(to_io)?
                .ok_or_else(|| io::Error::other(format!("not a directory: {path:?}")))?;
            if json {
                let arr: Vec<Value> = entries
                    .iter()
                    .map(|e| json!({ "name": e.name, "kind": node_kind_str(e.kind), "mode": format!("{:o}", e.mode) }))
                    .collect();
                let fetched = report(&vfs);
                println!(
                    "{}",
                    render_json(&json!({ "ok": true, "op": "ls", "path": path, "entries": arr, "fetched": fetched }))
                );
            } else {
                for e in &entries {
                    let marker = match e.kind {
                        NodeKind::Dir => "/",
                        NodeKind::Symlink => "@",
                        NodeKind::File => "",
                    };
                    println!("{}{marker}", e.name);
                }
                report(&vfs);
            }
        }
        "stat" => {
            let a = vfs
                .getattr(path)
                .map_err(to_io)?
                .ok_or_else(|| io::Error::other(format!("no such path: {path:?}")))?;
            if json {
                let fetched = report(&vfs);
                println!(
                    "{}",
                    render_json(&json!({
                        "ok": true, "op": "stat", "path": path,
                        "kind": node_kind_str(a.kind), "mode": format!("{:o}", a.mode), "size": a.size,
                        "fetched": fetched,
                    }))
                );
            } else {
                println!("{} {:o} {} bytes  {path}", node_kind_str(a.kind), a.mode, a.size);
                report(&vfs);
            }
        }
        "cat" => {
            let bytes = vfs
                .read(path)
                .map_err(to_io)?
                .ok_or_else(|| io::Error::other(format!("not a file: {path:?}")))?;
            if json {
                let fetched = report(&vfs);
                println!(
                    "{}",
                    render_json(&json!({ "ok": true, "op": "cat", "path": path, "size": bytes.len(), "fetched": fetched }))
                );
            } else {
                io::stdout().write_all(&bytes)?;
                report(&vfs);
            }
        }
        other => {
            return Err(io::Error::other(format!("unknown vfs op {other:?}; use ls|stat|cat")))
        }
    }
    Ok(())
}

fn node_kind_str(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Dir => "dir",
        NodeKind::File => "file",
        NodeKind::Symlink => "symlink",
    }
}

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
    // Attribute this work to the lessons the last brief served — so when this change is verified
    // green, those lessons earn a helpfulness bump (the flywheel's feedback edge).
    let informed = served_lessons(&_root);
    if !informed.is_empty() {
        repo.store().set_informed(&head, &informed).map_err(to_io)?;
    }
    // Best-effort: if a daemon with QUIC is up, broadcast the new lesson to the fleet so other
    // agents subscribed via `keel net-events` see it live (coordination over the wire).
    let _ = daemon_request(
        &_root,
        &json!({"op": "announce", "event": {"kind": "lesson", "file": task, "lesson": lesson, "change": head.to_hex()}}),
    );
    println!("learned on {} — future briefs on this file or its neighbors will surface it", short(&head.to_hex()));
    Ok(())
}

/// The lessons the most recent `keel brief` served, from `.keel/last_brief.json` (each identified
/// by the change it's attached to).
fn served_lessons(root: &Path) -> Vec<ObjectId> {
    let raw = match std::fs::read_to_string(root.join(".keel/last_brief.json")) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get("served_lessons")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|s| s.as_str()).filter_map(ObjectId::from_hex).collect())
        .unwrap_or_default()
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

/// `keel net-events <host:port>` — subscribe to a keeld/net-serve QUIC endpoint and print live
/// fleet-coordination events (agents briefing on files, lessons learned, changes landed) as they
/// arrive. The remote presence side of the flywheel: coordination over the wire, not just locally.
fn cmd_net_events(args: &[String]) -> io::Result<()> {
    let addr_s = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .ok_or_else(|| io::Error::other("usage: keel net-events <host:port>"))?;
    let addr: std::net::SocketAddr =
        addr_s.parse().map_err(|e| io::Error::other(format!("bad address {addr_s}: {e}")))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let client = keel_net::Client::connect(addr).await.map_err(|e| io::Error::other(e.to_string()))?;
        let mut events = client.subscribe().await.map_err(|e| io::Error::other(e.to_string()))?;
        eprintln!("keel net-events: subscribed to {addr} — waiting for coordination events (Ctrl-C to stop)");
        while let Some(bytes) = events.next().await {
            // events are JSON coordination messages; print each on its own line (pass through raw
            // if it isn't JSON we recognize).
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => println!("{}", serde_json::to_string(&v).unwrap_or_default()),
                Err(_) => println!("{}", String::from_utf8_lossy(&bytes)),
            }
        }
        eprintln!("keel net-events: stream closed");
        Ok(())
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

    // `--semantic`: instead of raw hunks, collapse the mechanical bulk and surface what needs eyes.
    if has(args, "--semantic") {
        return semantic_diff(&repo, &root, head, &changes, has(args, "--json"));
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

/// `keel native diff --semantic [--file <p>] [--json]` — the operation view of a change: every added
/// line is masked to a shape, same-shaped lines are collapsed as one mechanical edit, and a numeric
/// literal that a bulk group overwhelmingly agrees on but one site breaks is surfaced as an anomaly
/// (the class of bug a line diff buries). Deterministic, no model. See [`keel_store::semdiff`].
fn semantic_diff(
    repo: &Repo,
    root: &Path,
    head: Option<ObjectId>,
    changes: &[keel_store::PathChange],
    json: bool,
) -> io::Result<()> {
    let mut added: Vec<String> = Vec::new();
    let (mut files, mut binary) = (0usize, 0usize);
    for c in changes {
        let old = match head {
            Some(h) => repo.file_bytes_at(h, &c.path).map_err(to_io)?.unwrap_or_default(),
            None => Vec::new(),
        };
        let new = match c.kind {
            ChangeKind::Deleted => Vec::new(),
            _ => std::fs::read(root.join(&c.path)).unwrap_or_default(),
        };
        if old.contains(&0) || new.contains(&0) {
            binary += 1;
            continue; // binary file — nothing to mask
        }
        files += 1;
        let (o, n) = (String::from_utf8_lossy(&old), String::from_utf8_lossy(&new));
        for h in diff_lines(&o, &n) {
            for line in h.lines {
                if line.tag == Tag::Add {
                    added.push(line.text);
                }
            }
        }
    }
    let s = semantic_summarize(&added);

    if json {
        println!(
            "{}",
            render_json(&json!({
                "ok": true, "files": files, "binary_files": binary,
                "added_lines": s.added_lines, "substantive_lines": s.substantive_lines,
                "mechanical_lines": s.mechanical_lines, "anomalies": s.anomaly_count,
                "groups": semantic_groups_json(&s),
            }))
        );
        return Ok(());
    }

    let bin = if binary > 0 { format!(" ({binary} binary skipped)") } else { String::new() };
    println!("semantic diff · {files} file(s){bin} · {} added line(s)", s.added_lines);
    println!(
        "  {} substantive · {} mechanical · {} anomaly(ies)",
        s.substantive_lines, s.mechanical_lines, s.anomaly_count
    );
    print_semantic_body(&s);
    Ok(())
}

/// The JSON form of a semantic summary's groups (shared by `keel native diff --semantic` and
/// `keel walkthrough --semantic`).
fn semantic_groups_json(s: &SemanticSummary) -> Vec<Value> {
    s.groups
        .iter()
        .map(|g| {
            json!({
                "kind": if g.kind == GroupKind::Mechanical { "mechanical" } else { "substantive" },
                "shape": g.shape,
                "count": g.count(),
                "representative": g.representative(),
                "members": g.members,
                "anomalies": g.anomalies.iter().map(|a| json!({"text": a.text, "reason": a.reason})).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// `keel walkthrough <change> --semantic` — the operation view of a *committed* change (vs its
/// parent), under the session header. Same engine as `keel native diff --semantic`, but over the
/// change's added lines rather than the working tree, so reviewing an agent's change reads as
/// operations + anomalies instead of thousands of hunk lines.
#[allow(clippy::too_many_arguments)]
fn walkthrough_semantic(
    repo: &Repo,
    id: ObjectId,
    intent: &str,
    author: &str,
    timestamp: u64,
    verif: &str,
    task: Option<&str>,
    model: Option<&str>,
    lesson: Option<&str>,
    files: &[keel_store::PathChange],
    parent: Option<ObjectId>,
    json: bool,
) -> io::Result<()> {
    let mut added: Vec<String> = Vec::new();
    let (mut nfiles, mut binary) = (0usize, 0usize);
    for f in files {
        let old = match (parent, f.kind) {
            (_, ChangeKind::Added) => Vec::new(),
            (Some(p), _) => repo.file_bytes_at(p, &f.path).map_err(to_io)?.unwrap_or_default(),
            (None, _) => Vec::new(),
        };
        let new = match f.kind {
            ChangeKind::Deleted => Vec::new(),
            _ => repo.file_bytes_at(id, &f.path).map_err(to_io)?.unwrap_or_default(),
        };
        if old.contains(&0) || new.contains(&0) {
            binary += 1;
            continue;
        }
        nfiles += 1;
        let (o, n) = (String::from_utf8_lossy(&old), String::from_utf8_lossy(&new));
        for h in diff_lines(&o, &n) {
            for line in h.lines {
                if line.tag == Tag::Add {
                    added.push(line.text);
                }
            }
        }
    }
    let s = semantic_summarize(&added);

    if json {
        let mut out = json!({
            "ok": true, "view": "semantic", "change": id.to_hex(), "intent": intent,
            "author": author, "timestamp": timestamp, "verification": verif,
            "files": nfiles, "binary_files": binary, "added_lines": s.added_lines,
            "substantive_lines": s.substantive_lines, "mechanical_lines": s.mechanical_lines,
            "anomalies": s.anomaly_count, "groups": semantic_groups_json(&s),
        });
        if let Some(t) = task {
            out["task"] = json!(t);
        }
        if let Some(m) = model {
            out["model"] = json!(m);
        }
        if let Some(l) = lesson {
            out["lesson"] = json!(l);
        }
        println!("{}", render_json(&out));
        return Ok(());
    }

    let mark = match verif {
        "green" => "✓",
        "red" => "✗",
        _ => "·",
    };
    println!("walkthrough · {} {}  {intent}  (semantic)", mark, short(&id.to_hex()));
    println!("  by {author} · {} · verified {verif}", rel_time(timestamp));
    if let Some(t) = task {
        println!("  task:  {t}");
    }
    if let Some(m) = model {
        println!("  model: {m}");
    }
    if let Some(l) = lesson {
        println!("  lesson: {l}");
    }
    let bin = if binary > 0 { format!(" ({binary} binary skipped)") } else { String::new() };
    println!(
        "  scope: {nfiles} file(s){bin} · {} added · {} substantive · {} mechanical · {} anomaly(ies)",
        s.added_lines, s.substantive_lines, s.mechanical_lines, s.anomaly_count
    );
    print_semantic_body(&s);
    println!(
        "\n  → record a verdict: keel review --target {} --verdict <approve|request-changes|reject> [--human]",
        id.to_hex()
    );
    Ok(())
}

/// Print the substantive/mechanical body of a semantic summary — unique lines to read, then bulk
/// edits collapsed to a count + one representative, each anomaly flagged with its reason.
fn print_semantic_body(s: &SemanticSummary) {
    let subst: Vec<&ChangeGroup> = s.groups.iter().filter(|g| g.kind == GroupKind::Substantive).collect();
    let mech: Vec<&ChangeGroup> = s.groups.iter().filter(|g| g.kind == GroupKind::Mechanical).collect();
    if !subst.is_empty() {
        println!("\nsubstantive — unique changes to read:");
        for g in subst {
            for m in &g.members {
                println!("  + {m}");
            }
        }
    }
    if !mech.is_empty() {
        println!("\nmechanical — bulk edits, collapsed:");
        for g in mech {
            println!("  {}× `{}`   e.g. {}", g.count(), g.shape, g.representative());
            for a in &g.anomalies {
                println!("     ⚠ anomaly: {}  — {}", a.text, a.reason);
            }
        }
    }
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

    // Full DAG reachable from HEAD, in topological order (parents before children), each line:
    //   <hash> \x1f <space-separated parent hashes> \x1f <author> \x1f <ts> \x1f <subject>
    // Preserving every parent (not just the first) keeps merge commits and merged-in branches.
    let log = Command::new("git")
        .args(["-C", src, "log", "--topo-order", "--reverse", "--format=%H%x1f%P%x1f%an%x1f%at%x1f%s"])
        .output()?;
    if !log.status.success() {
        return Err(io::Error::other(format!("git log failed in {src} (not a git repo?)")));
    }
    let mut commits: Vec<(String, Vec<String>, String, u64, String)> = String::from_utf8_lossy(&log.stdout)
        .lines()
        .filter_map(|l| {
            let mut p = l.split('\u{1f}');
            let hash = p.next()?.to_string();
            let parents = p.next()?.split_whitespace().map(str::to_string).collect();
            let author = p.next()?.to_string();
            let ts = p.next()?.parse().unwrap_or(0);
            let msg = p.next().unwrap_or("").to_string();
            Some((hash, parents, author, ts, msg))
        })
        .collect();
    let total = commits.len();
    if commits.len() > limit {
        // Keep the most recent `limit` in topo order; any parent that falls outside the kept set is
        // dropped when mapped below (that commit becomes a root), so no edge dangles.
        commits = commits.split_off(total - limit);
        eprintln!("keel import: {total} commits found; importing the most recent {limit} (raise with --limit)");
    }

    // The branch tip to point at when done (keel id of git's HEAD commit).
    let head_hash = Command::new("git").args(["-C", src, "rev-parse", "HEAD"]).output()?;
    let head_hash = String::from_utf8_lossy(&head_hash.stdout).trim().to_string();

    // Hold the commit lock across the ENTIRE replay: the changes are stored but unreferenced until
    // the final `set_branch`, so a concurrent GC in between would sweep them (see Store::gc).
    let _lock = repo.store().commit_lock().map_err(to_io)?;
    let base = std::env::temp_dir().join(format!("keel-import-{}", std::process::id()));
    let mut map: std::collections::HashMap<String, keel_store::ObjectId> = std::collections::HashMap::new();
    let mut n = 0usize;
    for (hash, parents, author, ts, msg) in &commits {
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
        // Map git parents → keel ids (topo order guarantees parents are already imported); a parent
        // trimmed by --limit is simply omitted, making this commit a root on the keel side.
        let mapped: Vec<keel_store::ObjectId> = parents.iter().filter_map(|p| map.get(p).copied()).collect();
        // uncached snapshot: distinct trees at identical paths in rapid succession would racily
        // false-hit the mtime+size stat cache.
        let tree = keel_store::snapshot::snapshot_uncached(repo.store(), &work).map_err(to_io)?;
        let keel_id =
            repo.commit_tree_with_parents(tree, mapped, intent, author, *ts, None).map_err(to_io)?;
        map.insert(hash.clone(), keel_id);
        let _ = std::fs::remove_dir_all(&work); // one tree on disk at a time
        n += 1;
    }
    let _ = std::fs::remove_dir_all(&base);
    // Advance the branch to the keel id of git's HEAD (fall back to the last imported commit).
    let head_id = map.get(&head_hash).copied().or_else(|| commits.last().and_then(|c| map.get(&c.0).copied()));
    match head_id {
        Some(id) => {
            repo.set_branch(id).map_err(to_io)?;
            println!("imported {n} commits into {} · HEAD {}", store.display(), short(&id.to_hex()));
        }
        None => println!("nothing to import (no commits found)"),
    }
    Ok(())
}

/// `keel export <git-dir>` — mirror keel's history INTO a git repo (keel→git), the removable
/// coexistence bridge for human reviewers on GitHub during transition. keel stays the system of
/// record; this writes a git *copy*. Preserves author / message / date AND the full commit DAG
/// (merges keep every parent), using `git commit-tree` so the topology round-trips.
fn cmd_export(args: &[String]) -> io::Result<()> {
    use std::process::Command;
    let dst = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| io::Error::other("usage: keel export <git-dir> [--limit N]"))?;
    let limit: usize = flag(args, "--limit").and_then(|s| s.parse().ok()).unwrap_or(1000);
    let (_, store) = root_store(args)?;
    let repo = Repo::open(&store).map_err(to_io)?;

    let Some(head) = repo.head().map_err(to_io)? else {
        println!("nothing to export (empty repo)");
        return Ok(());
    };
    // All changes reachable from HEAD via every parent edge, topologically ordered (parents before
    // children) so each commit-tree can reference already-written parents.
    let mut order = topo_changes(&repo, head)?;
    if order.len() > limit {
        let total = order.len();
        order = order.split_off(total - limit); // keep the most recent `limit` (parents dropped below)
        eprintln!("keel export: {total} commits; exporting the most recent {limit}");
    }

    let dstp = PathBuf::from(dst);
    std::fs::create_dir_all(&dstp)?;
    let git = |a: &[&str]| Command::new("git").arg("-C").arg(&dstp).args(a).status();
    git(&["init", "-q"])?;

    let work = std::env::temp_dir().join(format!("keel-export-{}", std::process::id()));
    let mut map: std::collections::HashMap<keel_store::ObjectId, String> = std::collections::HashMap::new();
    let mut n = 0usize;
    let mut head_git = String::new();
    for id in &order {
        let c = repo.change(*id).map_err(to_io)?.ok_or_else(|| io::Error::other("missing change"))?;
        // Stage this change's tree into the git index → a git tree object.
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        repo.checkout_change(*id, &work).map_err(to_io)?;
        clear_worktree(&dstp)?;
        copy_dir(&work, &dstp)?;
        Command::new("git").arg("-C").arg(&dstp).args(["add", "-A"]).status()?;
        let tree_out = Command::new("git").arg("-C").arg(&dstp).arg("write-tree").output()?;
        if !tree_out.status.success() {
            return Err(io::Error::other(format!("git write-tree failed at keel change {}", short(&id.to_hex()))));
        }
        let tree_hash = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

        // commit-tree with every mapped parent (a parent trimmed by --limit is simply omitted).
        let date = format!("@{} +0000", c.timestamp);
        let intent = if c.intent.is_empty() { "(no message)".to_string() } else { c.intent.clone() };
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&dstp).args(["commit-tree", &tree_hash]);
        for p in &c.parents {
            if let Some(gp) = map.get(p) {
                cmd.args(["-p", gp]);
            }
        }
        let commit_out = cmd
            .args(["-m", &intent])
            .env("GIT_AUTHOR_NAME", &c.author)
            .env("GIT_AUTHOR_EMAIL", "keel@local")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", "keel-export")
            .env("GIT_COMMITTER_EMAIL", "keel@local")
            .env("GIT_COMMITTER_DATE", &date)
            .output()?;
        if !commit_out.status.success() {
            return Err(io::Error::other(format!("git commit-tree failed at keel change {}", short(&id.to_hex()))));
        }
        let git_hash = String::from_utf8_lossy(&commit_out.stdout).trim().to_string();
        head_git = git_hash.clone();
        map.insert(*id, git_hash);
        n += 1;
    }
    let _ = std::fs::remove_dir_all(&work);
    // Point the branch (and HEAD's tip on disk) at the exported HEAD commit.
    if let Some(h) = map.get(&head) {
        head_git = h.clone();
    }
    if !head_git.is_empty() {
        git(&["update-ref", &format!("refs/heads/{}", repo.branch()), &head_git])?;
        git(&["symbolic-ref", "HEAD", &format!("refs/heads/{}", repo.branch())])?;
        Command::new("git").arg("-C").arg(&dstp).args(["checkout", "-q", "-f", repo.branch()]).status()?;
    }
    println!("exported {n} commits to {dst} (git mirror; keel remains the system of record)");
    Ok(())
}

/// Changes reachable from `head` via every parent edge, in topological order (parents before
/// children) — the order `git commit-tree` needs so a parent is written before its child.
fn topo_changes(repo: &Repo, head: keel_store::ObjectId) -> io::Result<Vec<keel_store::ObjectId>> {
    use std::collections::HashMap;
    let mut order = Vec::new();
    let mut state: HashMap<keel_store::ObjectId, bool> = HashMap::new(); // present=visited, true=emitted
    let mut stack = vec![(head, false)];
    while let Some((id, processed)) = stack.pop() {
        if processed {
            if state.get(&id) != Some(&true) {
                order.push(id);
                state.insert(id, true);
            }
            continue;
        }
        if state.contains_key(&id) {
            continue;
        }
        state.insert(id, false);
        stack.push((id, true));
        if let Some(c) = repo.change(id).map_err(to_io)? {
            for p in c.parents {
                if !state.contains_key(&p) {
                    stack.push((p, false));
                }
            }
        }
    }
    Ok(order)
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
    // Feedback: reward/penalize the lessons that INFORMED this change (the flywheel learns which
    // retrieved lessons actually lead to green outcomes, and ranks them up for the next brief).
    let delta = if matches!(v, Verification::Green) { 1 } else { -1 };
    let informed = repo.store().informed(&id).map_err(to_io)?;
    for lesson in &informed {
        let _ = repo.store().bump_lesson_help(lesson, delta);
    }
    let mark = if matches!(v, Verification::Green) { "green ✓" } else { "red ✗" };
    let credited = if informed.is_empty() { String::new() } else { format!(" · {} lesson(s) {}", informed.len(), if delta > 0 { "credited" } else { "penalized" }) };
    println!("verified {} · {mark}{credited}", short(&id.to_hex()));
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
    // A payload reads as `erased` once its crypto-shred key is gone (right-to-erasure), `yes` if
    // present (plaintext blob or still-decryptable), `—` if the session never carried it.
    let status = |o: &Option<keel_store::ObjectId>| -> &'static str {
        match o {
            None => "—",
            Some(pid) => match repo.store().read_secret(pid) {
                Ok(keel_store::SecretState::Shredded) => "erased",
                _ => "yes",
            },
        }
    };
    println!("session for change {}", short(&id.to_hex()));
    println!("  task:      {}", s.task);
    println!("  model:     {}", s.model);
    if !s.lesson.is_empty() {
        println!("  lesson:    {}", s.lesson);
    }
    println!("  tokens:    in {} / out {}", s.tokens_in, s.tokens_out);
    println!("  verified:  {:?}", s.verification);
    // erasure-aware count for the payload vecs: N, or "N (erased)" once every entry's key is shredded.
    let vec_status = |v: &[keel_store::ObjectId]| -> String {
        if v.is_empty() {
            return "0".to_string();
        }
        let all_erased = v
            .iter()
            .all(|pid| matches!(repo.store().read_secret(pid), Ok(keel_store::SecretState::Shredded)));
        if all_erased {
            format!("{} (erased)", v.len())
        } else {
            v.len().to_string()
        }
    };
    println!("  prompts:   {}", status(&s.prompts));
    println!("  context:   {}", status(&s.context_served));
    println!("  tool_calls: {}   tool_results: {}", vec_status(&s.tool_calls), vec_status(&s.tool_results));
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
    let sock = keel_store::daemon_socket_path(root);
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

    #[test]
    fn is_test_path_spans_language_conventions() {
        for p in [
            "src/foo/tests/bar.rs",           // path segment (rust/tokio)
            "django/db/models/test_query.py", // test_ prefix (python/django)
            "prometheus/tsdb/head_test.go",   // _test suffix (go/prometheus)
            "src/vs/editor/parser.test.ts",   // .test. infix (ts/vscode)
            "app/__tests__/view.js",          // jest __tests__ dir
            "lib/thing_spec.rb",              // _spec suffix (ruby)
        ] {
            assert!(is_test_path(p), "should read as a test file: {p}");
        }
        for p in ["src/parser.rs", "kernel/sched/core.c", "latest.go", "src/contest.py"] {
            assert!(!is_test_path(p), "should NOT read as a test file: {p}");
        }
    }

    #[test]
    fn test_covers_matches_source_across_affixes() {
        assert!(test_covers("src/core.rs", "src/core_test.rs"));
        assert!(test_covers("a/parser.ts", "a/parser.test.ts"));
        assert!(test_covers("db/views.py", "db/tests/test_views.py")); // different dir still counts
        assert!(test_covers("tsdb/head.go", "tsdb/head_test.go"));
        // regression: a source whose own name starts with "spec"/"test" must still link to its
        // `_test` sibling — the old chained prefix+bare-suffix strip emptied the stem and returned
        // false on these standard pairings (review finding).
        assert!(test_covers("src/spec.go", "src/spec_test.go"));
        assert!(test_covers("src/spec_loader.rs", "src/spec_loader_test.rs"));
        assert!(test_covers("x/test.go", "x/test_test.go"));
        // non-matches: different stem must not link, and a lookalike name must not over-strip
        assert!(!test_covers("src/core.rs", "src/parser_test.rs"));
        assert!(!test_covers("src/core.rs", "src/other.rs"));
        assert!(!test_covers("src/la.rs", "src/latest.rs")); // "latest" must NOT strip to "la"
    }

    #[test]
    fn first_positional_skips_flags_and_value_flag_args() {
        let a = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // the value of --root must not be mistaken for the positional <change>
        assert_eq!(first_positional(&a(&["--root", "/repo", "abc123"])), Some("abc123"));
        assert_eq!(first_positional(&a(&["abc123", "--root", "/repo"])), Some("abc123"));
        assert_eq!(first_positional(&a(&["--json", "--full", "deadbeef"])), Some("deadbeef"));
        assert_eq!(first_positional(&a(&["--store", "/s", "--json"])), None);
        assert_eq!(first_positional(&a(&[])), None);
    }

    #[test]
    fn positionals_skips_value_flag_args_and_keeps_order() {
        let a = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // `keel vfs --root DIR <change> ls <path>` must yield [<change>, ls, <path>], not DIR first —
        // the bug the walkthrough review caught, here for a three-positional command.
        assert_eq!(positionals(&a(&["--root", "/repo", "abc", "ls", "src/x"])), ["abc", "ls", "src/x"]);
        assert_eq!(positionals(&a(&["abc", "cat", "f", "--json"])), ["abc", "cat", "f"]);
        assert_eq!(positionals(&a(&["--json"])), Vec::<&str>::new());
    }

    #[test]
    fn full_render_interleaves_each_hunk_header_with_its_own_body() {
        // Two hunks: in --full the human render must pair header→body, not dump all headers then all
        // bodies (review finding — the latter is unreadable for multi-hunk files).
        let v = json!({
            "change": "abc", "intent": "x", "author": "a", "timestamp": 0u64,
            "verification": "green", "files": 1, "added_lines": 2, "deleted_lines": 0,
            "proof_summary": {"verified": true, "covered": 0, "untested": 0, "tests_changed": 0},
            "blocks": [{
                "path": "f.rs", "kind": "modified", "binary": false,
                "added_lines": 2, "deleted_lines": 0,
                "hunks": ["@@ -1,1 +1,2 @@", "@@ -10,1 +11,2 @@"],
                "hunk_bodies": [[" ctx", "+first"], [" ctx2", "+second"]],
                "proof": {"status": "verified", "tests": [], "verification": "green"},
            }],
        });
        let h = render_walkthrough_human(&v);
        // each body line appears AFTER its own header and BEFORE the next header
        let p_h1 = h.find("@@ -1,1 +1,2 @@").unwrap();
        let p_first = h.find("+first").unwrap();
        let p_h2 = h.find("@@ -10,1 +11,2 @@").unwrap();
        let p_second = h.find("+second").unwrap();
        assert!(p_h1 < p_first && p_first < p_h2 && p_h2 < p_second, "hunks must interleave:\n{h}");
    }

    #[test]
    fn walkthrough_block_links_proof_and_flags_untested() {
        let tests = vec!["src/core_test.rs".to_string()];
        // a source file with a colocated test → covered, and the test is named
        let covered = walkthrough_block(
            "src/core.rs", ChangeKind::Modified, b"a\n", b"a\nb\n", &tests, "unverified", false,
        );
        assert_eq!(covered["proof"]["status"], "covered");
        assert_eq!(covered["proof"]["tests"][0], "src/core_test.rs");
        assert_eq!(covered["added_lines"], 1);

        // the test file itself is its own evidence
        let is_test = walkthrough_block(
            "src/core_test.rs", ChangeKind::Modified, b"", b"x\n", &tests, "unverified", false,
        );
        assert_eq!(is_test["proof"]["status"], "is-test");

        // a lone source file, change not green → untested (the block a reviewer must open)
        let untested = walkthrough_block(
            "src/alone.rs", ChangeKind::Added, b"", b"x\n", &[], "unverified", false,
        );
        assert_eq!(untested["proof"]["status"], "untested");

        // same file, but the change verified green → leans on the recorded verdict
        let verified = walkthrough_block(
            "src/alone.rs", ChangeKind::Added, b"", b"x\n", &[], "green", false,
        );
        assert_eq!(verified["proof"]["status"], "verified");
    }

    #[test]
    fn walkthrough_human_render_leads_with_proof_and_verdict_nudge() {
        let v = json!({
            "change": "abc123def456abc123def456abc123def456abc123def456abc123def456abcd",
            "intent": "add retry", "author": "acct:x", "timestamp": 0u64,
            "verification": "green", "files": 1, "added_lines": 2, "deleted_lines": 0,
            "task": "make the client retry",
            "proof_summary": {"verified": true, "covered": 1, "untested": 0, "tests_changed": 1},
            "blocks": [{
                "path": "src/client.rs", "kind": "modified", "binary": false,
                "added_lines": 2, "deleted_lines": 0, "hunks": ["@@ -1,1 +1,3 @@"],
                "proof": {"status": "covered", "tests": ["src/client_test.rs"], "verification": "green"},
            }],
        });
        let h = render_walkthrough_human(&v);
        assert!(h.contains("walkthrough · ✓"), "verdict mark in header: {h}");
        assert!(h.contains("task:  make the client retry"));
        assert!(h.contains("proof: exercised by src/client_test.rs"), "proof link before diff: {h}");
        assert!(h.contains("keel review --target abc123def456"), "closes with a verdict nudge: {h}");
    }

    #[test]
    fn decode_chunked_roundtrips_a_normal_body() {
        // "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n" → "hello world"
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw), Some(b"hello world".to_vec()));
    }

    #[test]
    fn decode_chunked_rejects_overflowing_chunk_size_without_panicking() {
        // An attacker-controlled chunk size near usize::MAX must not wrap the bounds check into a
        // panic (which, with panic=abort, would take down the whole `keel serve` process). It should
        // simply report "not fully arrived yet" (None).
        let raw = b"fffffffffffffff0\r\nshort";
        assert_eq!(decode_chunked(raw), None);
        // usize::MAX itself, and a value one below, both exercise the checked_add / end+2 guards.
        assert_eq!(decode_chunked(b"ffffffffffffffff\r\nx"), None);
    }

    // Build credential fixtures at RUNTIME from split parts so no full-credential literal ever lives
    // in the committed source — otherwise GitHub push-protection (correctly!) rejects the test data.
    // Each half is inert on its own; only the concatenation matches a secret shape, which is exactly
    // what the scrubber operates on.
    #[test]
    fn scrub_redacts_known_credential_shapes() {
        let secrets = [
            format!("sk-ant-{}", "api03-abcDEF1234567890abcDEF1234567890xyz"), // Anthropic
            format!("sk-proj-{}", "abcDEF1234567890abcDEF1234567890"),         // OpenAI project key
            format!("ghp_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab"),        // GitHub token
            format!("github_pat_{}", "11ABCDEF0123456789_abcDEFghiJKL"),        // GitHub fine-grained PAT
            format!("glpat-{}", "ABCDEF0123456789xyzw"),                        // GitLab PAT
            format!("AKIA{}", "IOSFODNN7EXAMPLE"),                              // AWS access key id
            format!("AIza{}", "SyD-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"),          // Google API key
            format!("xoxb-{}", "1234567890-ABCDEFabcdef"),                      // Slack bot token
        ];
        for s in &secrets {
            let (out, n) = scrub_secrets(&format!("key = {s} end"));
            assert_eq!(n, 1, "expected exactly one redaction for {s:?} → {out:?}");
            assert!(!out.contains(s.as_str()), "secret leaked through: {out:?}");
            assert!(out.contains("[REDACTED"), "no marker for {s:?}: {out:?}");
            assert!(out.starts_with("key = ") && out.ends_with(" end"), "surrounding text mangled: {out:?}");
        }
    }

    #[test]
    fn scrub_redacts_jwt_and_pem_but_preserves_context() {
        // assembled at runtime (see note above); each segment is inert alone
        let jwt = format!("{}.{}.{}", "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
                          "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c");
        let (out, n) = scrub_secrets(&format!("Authorization: Bearer {jwt}"));
        assert_eq!(n, 1);
        assert!(out.contains("[REDACTED-JWT]") && !out.contains(&jwt));
        assert!(out.starts_with("Authorization: Bearer "));

        let k = "PRIVATE KEY";
        let pem = format!("-----BEGIN RSA {k}-----\nMIIEpAIBAAKCAQEA\nfakefakefake\n-----END RSA {k}-----\n");
        let (out2, n2) = scrub_secrets(&format!("here it is:\n{pem}done"));
        assert_eq!(n2, 1);
        assert!(out2.contains("[REDACTED-PRIVATE-KEY]") && !out2.contains("MIIEpAIBAAKCAQEA"));
        assert!(out2.starts_with("here it is:\n") && out2.ends_with("done"));

        // TRUNCATED block (BEGIN present, no END — transcript cut off): must still redact the partial
        // key body, not leak it.
        let truncated = format!("log:\n-----BEGIN RSA {k}-----\nMIIEpAIBAAKCAQEAfakekeybytes");
        let (out3, n3) = scrub_secrets(&truncated);
        assert_eq!(n3, 1, "truncated PEM must be redacted: {out3:?}");
        assert!(out3.contains("[REDACTED-PRIVATE-KEY]") && !out3.contains("MIIEpAIBAAKCAQEA"));

        // A transcript that merely MENTIONS a BEGIN header (no END, no key body) must be BOUNDED to
        // the paragraph, not swallow everything to EOF — the tail after a blank line survives.
        let mention = format!("PEM files start with -----BEGIN RSA {k}----- then base64.\n\nUnrelated tail kept.");
        let (out4, _n4) = scrub_secrets(&mention);
        assert!(out4.contains("[REDACTED-PRIVATE-KEY]"), "the header line is redacted");
        assert!(out4.contains("Unrelated tail kept."), "content after the blank line is NOT swallowed: {out4:?}");
    }

    #[test]
    fn scrub_does_not_over_redact_hashes_or_code() {
        // the false-positive cases that make a generic scrubber unusable on agent transcripts:
        // git/BLAKE3 hashes, code assignments, model ids, and prose must all pass through untouched.
        let safe = [
            "commit cef17d88a6bfe55852dbb49f474f6330558f64a8",       // 40-hex git sha
            "blob c2c5fa841aadc9356dbfbe96640333451f6f9554adad8a95814cdfe05b5f335c", // 64-hex BLAKE3
            "let token = getToken(); const secret = readSecret();",  // code assignments
            "solver claude-opus-4-8, judge claude-sonnet-5",         // model ids
            "the password is stored in the vault, ask the admin",    // prose containing 'password'
            "function helper(x: number): number { return x * 2; }",  // ordinary code
            "sk-spinner and xox-menu are CSS classes",               // short prefix lookalikes
            "sk-learn-model-checkpoint-v2-final-report",             // long lowercase slug (sk- entropy gate)
            "sk-2024-01-annual-review-notes-attached-here",          // digits but no uppercase → not a key
        ];
        for s in safe {
            let (out, n) = scrub_secrets(s);
            assert_eq!(n, 0, "false positive on {s:?} → {out:?}");
            assert_eq!(out, s, "text changed with no redaction: {out:?}");
        }
    }

    #[test]
    fn scrub_value_recurses_and_counts() {
        let gh = format!("ghp_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab");
        let aws = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let anth = format!("sk-ant-{}", "api03-abcDEF1234567890abcDEF1234567890xyz");
        let mut v = json!({
            "task": format!("deploy with {gh}"),
            "tokens_in": 42,
            "tool_results": ["clean output", format!("leaked {aws} here")],
            "nested": { "prompts": anth }
        });
        let n = scrub_value(&mut v);
        assert_eq!(n, 3, "expected 3 redactions across nested fields");
        let dump = serde_json::to_string(&v).unwrap();
        assert!(!dump.contains("ghp_") && !dump.contains("AKIA") && !dump.contains("sk-ant-"));
        assert_eq!(v["tokens_in"], json!(42), "non-string fields untouched");
    }

    #[test]
    fn scrub_pii_redacts_emails_conservatively() {
        // redacts well-formed addresses, preserving surrounding text
        for (input, expect_left) in [
            ("contact user@example.com now", "contact "),
            ("cc a.b+tag@sub.domain.co.uk please", "cc "),
            ("reply to foo@bar.com.", "reply to "),        // sentence-final: trailing '.' must not leak
            ("(user@example.org)", "("),                    // wrapped in parens
        ] {
            let (out, n) = scrub_pii(input);
            assert_eq!(n, 1, "one email in {input:?} → {out:?}");
            assert!(out.contains("[REDACTED-EMAIL]") && !out.contains('@'), "{out:?}");
            assert!(out.starts_with(expect_left));
        }
        // the sentence-final case must keep the trailing punctuation OUTSIDE the redaction
        assert_eq!(scrub_pii("reply to foo@bar.com.").0, "reply to [REDACTED-EMAIL].");
        // does NOT redact non-emails: @mentions, missing TLD, bare '@', code
        for safe in ["ping @alice on the thread", "user@localhost has no tld", "email me @ the office", "a @ b", "arr[i]@2"] {
            let (out, n) = scrub_pii(safe);
            assert_eq!(n, 0, "false positive on {safe:?} → {out:?}");
            assert_eq!(out, safe);
        }
    }

    #[test]
    fn capture_policy_defaults_are_conservative() {
        let d = CapturePolicy::default();
        assert!(!d.erasable && !d.redact_pii && d.retain_prompts && d.retain_tool_results,
                "a repo with no policy file must behave exactly as before");
    }

    #[test]
    fn version_value_reports_crate_version_and_stable_shape() {
        let v = version_value();
        // the crate version is always known at compile time; the build identity always carries it
        assert_eq!(v["version"], json!(env!("CARGO_PKG_VERSION")));
        // shape is stable for consumers that pin/record it: these keys always exist and are typed
        assert!(v["git_commit"].is_string(), "git_commit must be a string (empty if built outside a checkout)");
        assert!(v["git_commit_short"].is_string(), "git_commit_short must be a string");
        assert!(v["dirty"].is_boolean(), "dirty must be a bool");
        // the short form never exceeds 12 chars and is a prefix of the full commit
        let full = v["git_commit"].as_str().unwrap();
        let short = v["git_commit_short"].as_str().unwrap();
        assert!(short.len() <= 12 && full.starts_with(short));
    }
}
