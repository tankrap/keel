//! keel CLI (Rust) — compiled, ~2ms startup. Ports the full local command set
//! over the git backend, emitting the same stable-key JSON contract as
//! keel.mjs (the reference oracle). Content primitives come from keel-core.

use keel_core::{canonical, J};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::{exit, Command};

// ── plumbing ────────────────────────────────────────────────────────────────
fn git(args: &[&str]) -> (i32, String) {
    match Command::new("git").args(args).output() {
        Ok(o) => (o.status.code().unwrap_or(1), String::from_utf8_lossy(&o.stdout).trim_end().to_string()),
        Err(_) => (1, String::new()),
    }
}
fn git_ok(args: &[&str]) -> bool { git(args).0 == 0 }

fn s(v: &str) -> J { J::S(v.to_string()) }
fn n(v: i64) -> J { J::N(v as f64) }

struct Ctx {
    cmd: String,
    full_est: i64,
}

fn emit(ctx: &Ctx, j: J) -> ! {
    let out = canonical(&j);
    println!("{}", out);
    // usage frame → .git/keel/metrics.jsonl (reuse the git-dir; no extra spawn beyond repo_info's)
    if ctx.cmd != "metrics" && ctx.cmd != "profile" {
        if let Some(info) = REPO.with(|r| r.borrow().clone()) {
            let d = PathBuf::from(&info.git_dir).join("keel");
            let _ = fs::create_dir_all(&d);
            let o = (out.len() as f64 / 4.0).ceil() as i64;
            let frame = if ctx.full_est > o {
                format!("{{\"c\":\"{}\",\"f\":{},\"o\":{}}}", ctx.cmd, ctx.full_est, o)
            } else {
                format!("{{\"c\":\"{}\",\"o\":{}}}", ctx.cmd, o)
            };
            let _ = append_line(&d.join("metrics.jsonl"), &frame);
        }
    }
    exit(0);
}
fn append_line(p: &PathBuf, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(p)?;
    writeln!(f, "{}", line)
}
fn die(code: &str, message: &str, fix: &str) -> ! {
    let mut o = vec![("error".to_string(), s(code)), ("message".to_string(), s(message))];
    if !fix.is_empty() {
        o.push(("fix".to_string(), s(fix)));
    }
    println!("{}", canonical(&J::O(o)));
    exit(1);
}

// memoized repo metadata (one rev-parse), like keel.mjs repoInfo()
#[derive(Clone)]
struct Info {
    git_dir: String,
    toplevel: String,
    branch: String,
    head: String,
}
thread_local! {
    static REPO: std::cell::RefCell<Option<Info>> = const { std::cell::RefCell::new(None) };
}
fn repo_info() -> Option<Info> {
    if let Some(i) = REPO.with(|r| r.borrow().clone()) {
        return Some(i);
    }
    let (c, out) = git(&["rev-parse", "--git-dir", "--show-toplevel"]);
    if c != 0 {
        return None;
    }
    let mut it = out.lines();
    let git_dir = it.next().unwrap_or("").to_string();
    let toplevel = it.next().unwrap_or("").to_string();
    let (bc, b) = git(&["symbolic-ref", "--short", "-q", "HEAD"]);
    let (hc, h) = git(&["rev-parse", "--short", "HEAD"]);
    let info = Info { git_dir, toplevel, branch: if bc == 0 { b } else { "HEAD".into() }, head: if hc == 0 { h } else { String::new() } };
    REPO.with(|r| *r.borrow_mut() = Some(info.clone()));
    Some(info)
}
fn require_repo() -> Info {
    repo_info().unwrap_or_else(|| die("E_NO_REPO", "not inside a repository", "cd into one, or: git init"))
}
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
fn diff_base(info: &Info) -> &'static str { if info.head.is_empty() { EMPTY_TREE } else { "HEAD" } }
fn keel_dir(info: &Info) -> PathBuf {
    let d = PathBuf::from(&info.git_dir).join("keel");
    let _ = fs::create_dir_all(&d);
    d
}

// ── flags ─────────────────────────────────────────────────────────────────
fn flag(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == name) {
        args.remove(i);
        true
    } else {
        false
    }
}
fn opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    if let Some(i) = args.iter().position(|a| a == name) {
        let v = args.get(i + 1).cloned();
        args.drain(i..=(i + 1).min(args.len() - 1));
        v
    } else {
        None
    }
}

// ── st ──────────────────────────────────────────────────────────────────────
fn numstat(base: &str) -> HashMap<String, (i64, i64)> {
    let (_, out) = git(&["diff", "--numstat", base]);
    let mut m = HashMap::new();
    for line in out.lines() {
        let c: Vec<&str> = line.splitn(3, '\t').collect();
        if c.len() == 3 {
            m.insert(c[2].to_string(), (c[0].parse().unwrap_or(0), c[1].parse().unwrap_or(0)));
        }
    }
    m
}
fn line_count(p: &str) -> i64 {
    fs::read_to_string(p).map(|c| c.matches('\n').count() as i64).unwrap_or(0)
}

fn cmd_st(_args: &mut [String]) -> ! {
    let ctx = Ctx { cmd: "st".into(), full_est: 0 };
    let info = require_repo();
    let (_, out) = git(&["status", "--porcelain=v2", "--branch", "--untracked-files=all"]);
    let (mut ahead, mut behind) = (0i64, 0i64);
    let mut files: Vec<(String, String)> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    for line in out.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut it = ab.split_whitespace();
            ahead = it.next().and_then(|x| x.strip_prefix('+')).and_then(|x| x.parse().ok()).unwrap_or(0);
            behind = it.next().and_then(|x| x.strip_prefix('-')).and_then(|x| x.parse().ok()).unwrap_or(0);
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let p: Vec<&str> = line.splitn(9, ' ').collect();
            let xy = p.get(1).unwrap_or(&"..").replace('.', "");
            let path = p.get(8).unwrap_or(&"").split('\t').next().unwrap_or("").to_string();
            files.push((xy, path));
        } else if let Some(u) = line.strip_prefix("u ") {
            if let Some(x) = u.rsplit(' ').next() { conflicts.push(x.to_string()); }
        } else if let Some(p) = line.strip_prefix("? ") {
            files.push(("A?".into(), p.to_string()));
        }
    }
    let ns = numstat(diff_base(&info));
    let mut rows: Vec<(String, String, i64, i64)> = files
        .into_iter()
        .map(|(st, p)| {
            let (a, d) = ns.get(&p).copied().unwrap_or_else(|| if st == "A?" { (line_count(&p), 0) } else { (0, 0) });
            (st, p, a, d)
        })
        .collect();
    rows.sort_by(|x, y| x.1.cmp(&y.1));

    let mut o: Vec<(String, J)> = vec![
        ("branch".into(), s(&info.branch)),
        ("cols".into(), J::A(vec![s("s"), s("p"), s("+"), s("-")])),
        ("files".into(), J::A(rows.iter().map(|(st, p, a, d)| J::A(vec![s(st), s(p), n(*a), n(*d)])).collect())),
    ];
    if ahead != 0 || behind != 0 {
        o.push(("ahead".into(), n(ahead)));
        o.push(("behind".into(), n(behind)));
    }
    if !conflicts.is_empty() {
        conflicts.sort();
        o.push(("conflicts".into(), J::A(conflicts.iter().map(|c| s(c)).collect())));
    }
    emit(&ctx, J::O(o));
}

// ── d (semantic digest) ──────────────────────────────────────────────────────
fn cmd_d(args: &mut Vec<String>) -> ! {
    let info = require_repo();
    let usage = flag(args, "--usage");
    let full = flag(args, "--full");
    let reshow = flag(args, "--reshow");
    let budget: i64 = opt(args, "--budget")
        .or_else(|| std::env::var("KEEL_BUDGET").ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let budget_chars = budget * 4;
    let paths: Vec<String> = args.iter().skip(1).cloned().collect();

    let base = diff_base(&info);
    let mut dargs = vec!["diff", "--no-color", base];
    if !paths.is_empty() {
        dargs.push("--");
        for p in &paths {
            dargs.push(p);
        }
    }
    let (_, patch) = git(&dargs);
    let mut lsargs = vec!["ls-files", "--others", "--exclude-standard"];
    if !paths.is_empty() {
        lsargs.push("--");
        for p in &paths {
            lsargs.push(p);
        }
    }
    let untracked: Vec<String> = git(&lsargs).1.lines().map(|s| s.to_string()).collect();

    let fn_def = Regex::new(r"^([+-])\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:function\s*\*?\s*([A-Za-z0-9_$]+)|def\s+([A-Za-z0-9_]+)|(?:pub\s+)?fn\s+([A-Za-z0-9_]+)|func\s+(?:\([^)]*\)\s*)?([A-Za-z0-9_]+))").unwrap();
    let hunk = Regex::new(r"^@@ .* @@ (.*)$").unwrap();
    let fname = Regex::new(r"([A-Za-z0-9_$]+)\s*\(").unwrap();

    // per-file: context set + defs (plus/minus) → tagged fns
    struct FileAcc { ctx: Vec<String>, defs: Vec<(String, bool, bool)> }
    let mut fmap: Vec<(String, FileAcc)> = Vec::new();
    let mut cur: Option<usize> = None;
    let file_re = Regex::new(r"^diff --git a/.* b/(.*)$").unwrap();
    for line in patch.lines() {
        if let Some(c) = file_re.captures(line) {
            fmap.push((c[1].to_string(), FileAcc { ctx: vec![], defs: vec![] }));
            cur = Some(fmap.len() - 1);
            continue;
        }
        let Some(ci) = cur else { continue };
        if let Some(h) = hunk.captures(line) {
            let t = h[1].trim();
            if !t.is_empty() {
                fmap[ci].1.ctx.push(t.chars().take(60).collect());
            }
            continue;
        }
        if let Some(m) = fn_def.captures(line) {
            let name = m.get(2).or(m.get(3)).or(m.get(4)).or(m.get(5)).map(|x| x.as_str().to_string());
            if let Some(name) = name {
                let plus = &m[1] == "+";
                let defs = &mut fmap[ci].1.defs;
                if let Some(e) = defs.iter_mut().find(|(nm, _, _)| *nm == name) {
                    if plus { e.1 = true } else { e.2 = true }
                } else {
                    defs.push((name, plus, !plus));
                }
            }
        }
    }
    let fns_of = |a: &FileAcc| -> String {
        let mut tagged: Vec<(String, String)> = Vec::new();
        for (name, p, m) in &a.defs {
            let tag = if *p && *m { format!("{}:sig", name) } else if *p { format!("{}:new", name) } else { format!("{}:gone", name) };
            tagged.push((name.clone(), tag));
        }
        for c in &a.ctx {
            let nm = fname.captures(c).map(|x| x[1].to_string()).unwrap_or_else(|| c.clone());
            if !tagged.iter().any(|(k, _)| *k == nm) {
                tagged.push((nm.clone(), nm));
            }
        }
        let mut vals: Vec<String> = tagged.into_iter().map(|(_, v)| v).collect();
        vals.sort();
        vals.join(" ")
    };

    let ns = numstat(base);
    let mut rows: Vec<(String, i64, i64, String)> = Vec::new();
    let mut keys: Vec<&String> = fmap.iter().map(|(k, _)| k).collect();
    keys.sort();
    for k in keys {
        let acc = &fmap.iter().find(|(kk, _)| kk == k).unwrap().1;
        let (a, d) = ns.get(k).copied().unwrap_or((0, 0));
        rows.push((k.clone(), a, d, fns_of(acc)));
    }
    let mut ut = untracked.clone();
    ut.sort();
    for p in &ut {
        rows.push((p.clone(), line_count(p), 0, "(new)".into()));
    }

    let mk_files = |rows: &[(String, i64, i64, String)]| -> J {
        J::A(rows.iter().map(|(p, a, d, f)| J::A(vec![s(p), n(*a), n(*d), s(f)])).collect())
    };
    let mut o: Vec<(String, J)> = vec![
        ("cols".into(), J::A(vec![s("p"), s("+"), s("-"), s("fns")])),
        ("files".into(), mk_files(&rows)),
    ];

    // full-est for the counterfactual (always, for metrics)
    let full_dump = patch.len() as i64 + ut.iter().map(|p| line_count(p) * 30).sum::<i64>();
    let full_est = (full_dump as f64 / 4.0).ceil() as i64;

    let shown_path = keel_dir(&info).join("shown.json");
    let mut shown: HashMap<String, String> = fs::read_to_string(&shown_path).ok().and_then(|c| parse_shown(&c)).unwrap_or_default();

    if full || !paths.is_empty() {
        let per_file: Vec<&str> = split_diff(&patch);
        let mut patches: Vec<J> = Vec::new();
        let mut elided: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let base_len = canonical(&J::O(o.clone())).len() as i64;
        let mut spent = base_len;
        for chunk in per_file {
            let name = file_re.captures(chunk.lines().next().unwrap_or("")).map(|c| c[1].to_string()).unwrap_or_else(|| "?".into());
            let wh = worktree_hash(&name);
            if !wh.is_empty() && shown.get(&name) == Some(&wh) && !reshow {
                seen.push(name);
                continue;
            }
            if spent + chunk.len() as i64 <= budget_chars {
                patches.push(J::O(vec![("p".into(), s(&name)), ("patch".into(), s(chunk.trim_end()))]));
                spent += chunk.len() as i64;
                if !wh.is_empty() {
                    shown.insert(name, wh);
                }
            } else {
                elided.push(name);
            }
        }
        o.push(("patches".into(), J::A(patches)));
        if !seen.is_empty() {
            o.push(("seen".into(), J::O(vec![("files".into(), J::A(seen.iter().map(|x| s(x)).collect())), ("note".into(), s("unchanged since last shown; --reshow to resend"))])));
        }
        if !elided.is_empty() {
            o.push(("elided".into(), J::O(vec![("files".into(), J::A(elided.iter().map(|x| s(x)).collect())), ("expand".into(), s(&format!("d {} --budget {}", elided[0], budget * 4)))])));
        }
        let _ = fs::write(&shown_path, write_shown(&shown));
    } else {
        let full_json_len = canonical(&J::O(o.clone())).len() as i64;
        if full_json_len > budget_chars && rows.len() > 3 {
            let keep = std::cmp::max(3, rows.len() / 4);
            o[1] = ("files".into(), mk_files(&rows[..keep]));
            o.push(("elided".into(), J::O(vec![("count".into(), n((rows.len() - keep) as i64)), ("expand".into(), s(&format!("d --budget {}", budget * 4)))])));
        }
    }

    if usage {
        let out_est = (canonical(&J::O(o.clone())).len() as f64 / 4.0).ceil() as i64;
        o.push(("usage".into(), J::O(vec![("full_dump_est".into(), n(full_est)), ("out_est".into(), n(out_est))])));
    }
    emit(&Ctx { cmd: "d".into(), full_est }, J::O(o));
}
fn split_diff(patch: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut bounds = vec![0usize];
    for (i, _) in patch.match_indices("\ndiff --git ") {
        bounds.push(i + 1);
    }
    bounds.push(patch.len());
    for w in bounds.windows(2) {
        let seg = &patch[w[0]..w[1]];
        if seg.starts_with("diff --git ") {
            out.push(seg);
        }
    }
    out
}
fn worktree_hash(p: &str) -> String {
    match fs::read(p) {
        Ok(b) => keel_core::hash_hex(&b)[..16].to_string(),
        Err(_) => String::new(),
    }
}
fn parse_shown(c: &str) -> Option<HashMap<String, String>> {
    // minimal {"path":"hash",...} reader
    let mut m = HashMap::new();
    let t = c.trim().trim_start_matches('{').trim_end_matches('}');
    for pair in t.split("\",\"") {
        let kv: Vec<&str> = pair.splitn(2, "\":\"").collect();
        if kv.len() == 2 {
            m.insert(kv[0].trim_matches('"').to_string(), kv[1].trim_matches('"').to_string());
        }
    }
    Some(m)
}
fn write_shown(m: &HashMap<String, String>) -> String {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let items: Vec<String> = keys.iter().map(|k| format!("\"{}\":\"{}\"", k, m[*k])).collect();
    format!("{{{}}}", items.join(","))
}

// ── save / undo ──────────────────────────────────────────────────────────────
fn oplog(info: &Info) -> PathBuf { keel_dir(info).join("oplog.jsonl") }

fn cmd_save(args: &mut [String]) -> ! {
    let info = require_repo();
    let msg = args.get(1).cloned().unwrap_or_default();
    if msg.is_empty() {
        die("E_USAGE", "save needs a message", "save \"what changed\"");
    }
    let before = info.head.clone();
    git(&["add", "-A"]);
    if git_ok(&["diff", "--cached", "--quiet"]) && !before.is_empty() {
        emit(&Ctx { cmd: "save".into(), full_est: 0 }, J::O(vec![("id".into(), s(&before)), ("noop".into(), J::B(true))]));
    }
    let (c, err) = git(&["commit", "-q", "-m", &msg]);
    if c != 0 {
        die("E_SAVE", &err.chars().take(200).collect::<String>(), "check repo state with: st");
    }
    let after = git(&["rev-parse", "--short", "HEAD"]).1;
    let _ = append_line(&oplog(&info), &format!("{{\"after\":\"{}\",\"before\":\"{}\",\"op\":\"save\"}}", after, before));
    emit(&Ctx { cmd: "save".into(), full_est: 0 }, J::O(vec![("id".into(), s(&after))]));
}

fn cmd_undo(_args: &mut [String]) -> ! {
    let info = require_repo();
    let content = fs::read_to_string(oplog(&info)).unwrap_or_default();
    let mut lines: Vec<String> = content.lines().filter(|l| !l.trim().is_empty()).map(|s| s.to_string()).collect();
    let Some(last) = lines.pop() else {
        die("E_NOTHING", "no keel operations recorded to undo", "");
    };
    let field = |k: &str| -> String {
        let key = format!("\"{}\":\"", k);
        last.find(&key).map(|i| { let start = i + key.len(); last[start..].find('"').map(|j| last[start..start + j].to_string()).unwrap_or_default() }).unwrap_or_default()
    };
    let after = field("after");
    let before = field("before");
    if after != info.head {
        die("E_STALE", "HEAD moved since the last keel operation; refusing", "inspect with: log");
    }
    if before.is_empty() {
        die("E_UNSUPPORTED", "cannot undo the root commit", "");
    }
    let (c, err) = git(&["reset", "--soft", &before]);
    if c != 0 {
        die("E_UNDO", &err.chars().take(200).collect::<String>(), "");
    }
    let rest = lines.join("\n");
    let _ = fs::write(oplog(&info), if rest.is_empty() { String::new() } else { format!("{}\n", rest) });
    emit(&Ctx { cmd: "undo".into(), full_est: 0 }, J::O(vec![("head".into(), s(&before)), ("undone".into(), s("save"))]));
}

// ── log ───────────────────────────────────────────────────────────────────────
fn cmd_log(args: &mut Vec<String>) -> ! {
    require_repo();
    let nn = opt(args, "-n").and_then(|v| v.parse::<i64>().ok()).unwrap_or(10);
    let grep = opt(args, "--grep");
    let range = args.get(1).cloned();
    let fmt = format!("-n{}", nn);
    let mut a = vec!["log", fmt.as_str(), "--format=%h\t%s"];
    let g;
    if let Some(gr) = &grep {
        g = format!("--grep={}", gr);
        a.push(&g);
    }
    if let Some(r) = &range {
        a.push(r);
    }
    let (c, out) = git(&a);
    if c != 0 {
        die("E_LOG", "log failed", "");
    }
    let commits: Vec<J> = out.lines().filter(|l| !l.is_empty()).map(|l| {
        let i = l.find('\t').unwrap_or(l.len());
        J::A(vec![s(&l[..i]), s(l.get(i + 1..).unwrap_or(""))])
    }).collect();
    emit(&Ctx { cmd: "log".into(), full_est: 0 }, J::O(vec![("cols".into(), J::A(vec![s("id"), s("s")])), ("commits".into(), J::A(commits))]));
}

// ── fix / sync ─────────────────────────────────────────────────────────────
fn conflicted_files() -> Vec<String> {
    let mut f: Vec<String> = git(&["diff", "--name-only", "--diff-filter=U"]).1.lines().map(|s| s.to_string()).collect();
    f.sort();
    f
}
fn cmd_fix(args: &mut Vec<String>) -> ! {
    require_repo();
    if flag(args, "--continue") {
        let (c, _) = git(&["-c", "core.editor=true", "rebase", "--continue"]);
        if c != 0 {
            die("E_CONFLICT", &format!("still conflicted: {}", conflicted_files().join(" ")), "edit the files, then: fix --continue");
        }
        emit(&Ctx { cmd: "fix".into(), full_est: 0 }, J::O(vec![("fixed".into(), J::B(true))]));
    }
    if flag(args, "--abort") {
        git(&["rebase", "--abort"]);
        emit(&Ctx { cmd: "fix".into(), full_est: 0 }, J::O(vec![("aborted".into(), J::B(true))]));
    }
    emit(&Ctx { cmd: "fix".into(), full_est: 0 }, J::O(vec![("conflicts".into(), J::A(conflicted_files().iter().map(|x| s(x)).collect()))]));
}
fn cmd_sync(args: &mut Vec<String>) -> ! {
    require_repo();
    let rebase = flag(args, "--rebase");
    let (uc, up) = git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    if uc != 0 {
        die("E_NO_UPSTREAM", "this branch has no upstream to sync with", "set one: git branch --set-upstream-to=<remote>/<branch>");
    }
    let remote = up.split('/').next().unwrap_or("origin").to_string();
    if !git_ok(&["fetch", "--quiet", &remote]) {
        die("E_FETCH", "fetch failed", "check the remote/network, then retry: sync");
    }
    let (_, cnt) = git(&["rev-list", "--left-right", "--count", &format!("HEAD...{}", up)]);
    let mut it = cnt.split_whitespace();
    let ahead: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let behind: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    if behind > 0 {
        if !git_ok(&["diff", "--quiet"]) || !git_ok(&["diff", "--cached", "--quiet"]) {
            die("E_DIRTY", "working tree has unsaved changes; syncing would clobber them", "save \"wip\" first, then: sync");
        }
        if ahead == 0 {
            if !git_ok(&["merge", "--ff-only", "--quiet", &up]) {
                die("E_SYNC", "fast-forward failed", "");
            }
        } else if !rebase {
            die("E_DIVERGED", &format!("local is {} ahead and {} behind {}", ahead, behind, up), "sync --rebase");
        } else {
            let (c, _) = git(&["-c", "core.editor=true", "rebase", &up]);
            if c != 0 {
                die("E_CONFLICT", &format!("rebase onto {} hit conflicts: {}", up, conflicted_files().join(" ")), "edit the files, then: fix --continue  (or: fix --abort)");
            }
        }
    }
    let mut pushed = 0;
    if ahead > 0 {
        if !git_ok(&["push", "--quiet", &remote, "HEAD"]) {
            die("E_PUSH", "push rejected", "sync again (upstream may have moved)");
        }
        pushed = ahead;
    }
    emit(&Ctx { cmd: "sync".into(), full_est: 0 }, J::O(vec![("pulled".into(), n(behind)), ("pushed".into(), n(pushed)), ("synced".into(), J::B(true))]));
}

// ── profile / metrics ────────────────────────────────────────────────────────
fn cmd_profile(_args: &mut [String]) -> ! {
    // defaults < preset < machine file < env  (value, source)
    let mut val: Vec<(&str, String, String)> = vec![
        ("budget", "2000".into(), "default".into()),
        ("cursor", "true".into(), "default".into()),
        ("preset", "none".into(), "default".into()),
        ("render", "auto".into(), "default".into()),
    ];
    let set = |val: &mut Vec<(&str, String, String)>, k: &str, v: String, src: String| {
        if let Some(e) = val.iter_mut().find(|(kk, _, _)| *kk == k) {
            e.1 = v;
            e.2 = src;
        }
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let machine: HashMap<String, String> = fs::read_to_string(format!("{}/.keel/profile.json", home)).ok().and_then(|c| parse_shown(&c)).unwrap_or_default();
    let preset = std::env::var("KEEL_PROFILE").ok().or_else(|| machine.get("preset").cloned());
    if let Some(p) = &preset {
        let src = if std::env::var("KEEL_PROFILE").is_ok() { "env:KEEL_PROFILE" } else { "machine" };
        if p == "agent" {
            set(&mut val, "budget", "2000".into(), format!("preset:{}", p));
            set(&mut val, "render", "json".into(), format!("preset:{}", p));
        } else if p == "human" {
            set(&mut val, "budget", "8000".into(), format!("preset:{}", p));
            set(&mut val, "render", "human".into(), format!("preset:{}", p));
        }
        set(&mut val, "preset", p.clone(), src.into());
    }
    for (mk, mv) in &machine {
        if ["budget", "render", "cursor"].contains(&mk.as_str()) {
            set(&mut val, mk, mv.clone(), "machine".into());
        }
    }
    if let Ok(b) = std::env::var("KEEL_BUDGET") {
        set(&mut val, "budget", b, "env:KEEL_BUDGET".into());
    }
    if let Ok(r) = std::env::var("KEEL_RENDER") {
        set(&mut val, "render", r, "env:KEEL_RENDER".into());
    }
    val.sort_by(|a, b| a.0.cmp(b.0));
    let rows: Vec<J> = val.iter().map(|(k, v, src)| {
        let vj = if let Ok(nv) = v.parse::<i64>() { n(nv) } else if v == "true" || v == "false" { J::B(v == "true") } else { s(v) };
        J::A(vec![s(k), vj, s(src)])
    }).collect();
    emit(&Ctx { cmd: "profile".into(), full_est: 0 }, J::O(vec![("cols".into(), J::A(vec![s("k"), s("v"), s("src")])), ("profile".into(), J::A(rows))]));
}

fn cmd_metrics(_args: &mut [String]) -> ! {
    let info = require_repo();
    let content = fs::read_to_string(keel_dir(&info).join("metrics.jsonl")).unwrap_or_default();
    let mut agg: Vec<(String, i64, i64, i64)> = Vec::new(); // verb, calls, out, displaced
    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let get_s = |k: &str| -> String {
            let key = format!("\"{}\":\"", k);
            line.find(&key).map(|i| { let s0 = i + key.len(); line[s0..].find('"').map(|j| line[s0..s0 + j].to_string()).unwrap_or_default() }).unwrap_or_default()
        };
        let get_n = |k: &str| -> i64 {
            let key = format!("\"{}\":", k);
            line.find(&key).map(|i| { let s0 = i + key.len(); let end = line[s0..].find(|c: char| !c.is_ascii_digit()).map(|j| s0 + j).unwrap_or(line.len()); line[s0..end].parse().unwrap_or(0) }).unwrap_or(0)
        };
        let c = get_s("c");
        let o = get_n("o");
        let f = get_n("f");
        if let Some(e) = agg.iter_mut().find(|(v, _, _, _)| *v == c) {
            e.1 += 1;
            e.2 += o;
            e.3 += if f > o { f - o } else { 0 };
        } else {
            agg.push((c, 1, o, if f > o { f - o } else { 0 }));
        }
    }
    agg.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut tc, mut to, mut td) = (0, 0, 0);
    let verbs: Vec<J> = agg.iter().map(|(v, c, o, d)| { tc += c; to += o; td += d; J::A(vec![s(v), n(*c), n(*o), n(*d)]) }).collect();
    emit(&Ctx { cmd: "metrics".into(), full_est: 0 }, J::O(vec![
        ("cols".into(), J::A(vec![s("verb"), s("calls"), s("tokens_out"), s("displaced")])),
        ("totals".into(), J::O(vec![("calls".into(), n(tc)), ("displaced".into(), n(td)), ("tokens_out".into(), n(to))])),
        ("verbs".into(), J::A(verbs)),
    ]));
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.to_string()).as_deref() {
        Some("st") => cmd_st(&mut args),
        Some("d") => cmd_d(&mut args),
        Some("save") => cmd_save(&mut args),
        Some("log") => cmd_log(&mut args),
        Some("undo") => cmd_undo(&mut args),
        Some("fix") => cmd_fix(&mut args),
        Some("sync") => cmd_sync(&mut args),
        Some("profile") => cmd_profile(&mut args),
        Some("metrics") => cmd_metrics(&mut args),
        Some("version") | Some("--version") => println!("{{\"core\":\"keel-core\",\"keel\":\"0.2.0-rust\"}}"),
        Some(other) => die("E_USAGE", &format!("unknown command: {}", other), "server commands (link/push/pull/clone/id/batch) still on keel.mjs"),
        None => println!("keel (rust) — st d save sync fix log undo profile metrics. Server cmds on keel.mjs during the port."),
    }
}
