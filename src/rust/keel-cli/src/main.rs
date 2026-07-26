//! keel CLI (Rust) — compiled binary, ~1-2ms startup vs Node's ~28ms floor.
//! v0 ports the hot read path (st) over the git backend, emitting the same
//! stable-key JSON contract as keel.mjs. Shares keel-core primitives.

use std::process::{exit, Command};

fn git(args: &[&str]) -> (i32, String) {
    match Command::new("git").args(args).output() {
        Ok(o) => (o.status.code().unwrap_or(1), String::from_utf8_lossy(&o.stdout).trim_end().to_string()),
        Err(_) => (1, String::new()),
    }
}

fn die(code: &str, message: &str, fix: &str) -> ! {
    if fix.is_empty() {
        println!("{{\"error\":\"{}\",\"message\":\"{}\"}}", code, message);
    } else {
        println!("{{\"error\":\"{}\",\"fix\":\"{}\",\"message\":\"{}\"}}", code, fix, message);
    }
    exit(1);
}

struct Info {
    branch: String,
    head: String,
}
fn repo_info() -> Option<Info> {
    let (c, _) = git(&["rev-parse", "--git-dir", "--show-toplevel"]);
    if c != 0 {
        return None;
    }
    let (bc, b) = git(&["symbolic-ref", "--short", "-q", "HEAD"]);
    let (hc, h) = git(&["rev-parse", "--short", "HEAD"]);
    Some(Info {
        branch: if bc == 0 { b } else { "HEAD".into() },
        head: if hc == 0 { h } else { String::new() },
    })
}

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn status_files() -> (i64, i64, Vec<(String, String)>, Vec<String>) {
    let (_, out) = git(&["status", "--porcelain=v2", "--branch", "--untracked-files=all"]);
    let (mut ahead, mut behind) = (0i64, 0i64);
    let mut files = Vec::new();
    let mut conflicts = Vec::new();
    for line in out.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut it = ab.split_whitespace();
            ahead = it.next().and_then(|s| s.strip_prefix('+')).and_then(|s| s.parse().ok()).unwrap_or(0);
            behind = it.next().and_then(|s| s.strip_prefix('-')).and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let parts: Vec<&str> = line.splitn(9, ' ').collect();
            let xy = parts.get(1).unwrap_or(&"..").replace('.', "");
            let path = parts.get(8).unwrap_or(&"").split('\t').next().unwrap_or("").to_string();
            files.push((xy, path));
        } else if let Some(u) = line.strip_prefix("u ") {
            if let Some(p) = u.rsplit(' ').next() {
                conflicts.push(p.to_string());
            }
        } else if let Some(p) = line.strip_prefix("? ") {
            files.push(("A?".into(), p.to_string()));
        }
    }
    (ahead, behind, files, conflicts)
}

fn numstat(base: &str) -> std::collections::HashMap<String, (i64, i64)> {
    let (_, out) = git(&["diff", "--numstat", base]);
    let mut m = std::collections::HashMap::new();
    for line in out.lines() {
        let cols: Vec<&str> = line.splitn(3, '\t').collect();
        if cols.len() == 3 {
            m.insert(cols[2].to_string(), (cols[0].parse().unwrap_or(0), cols[1].parse().unwrap_or(0)));
        }
    }
    m
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn cmd_st() {
    let info = repo_info().unwrap_or_else(|| die("E_NO_REPO", "not inside a repository", "cd into one, or: git init"));
    let (ahead, behind, files, conflicts) = status_files();
    let base = if info.head.is_empty() { EMPTY_TREE } else { "HEAD" };
    let ns = numstat(base);
    let mut rows: Vec<(String, String, i64, i64)> = files
        .into_iter()
        .map(|(s, p)| {
            let (a, d) = ns.get(&p).copied().unwrap_or((0, 0));
            (s, p, a, d)
        })
        .collect();
    rows.sort_by(|x, y| x.1.cmp(&y.1));

    // key-sorted to match keel.mjs sortKeys(): ahead, behind, branch, cols, conflicts, files
    let mut parts: Vec<(&str, String)> = Vec::new();
    parts.push(("branch", json_str(&info.branch)));
    parts.push(("cols", "[\"s\",\"p\",\"+\",\"-\"]".into()));
    let frows: Vec<String> =
        rows.iter().map(|(s, p, a, d)| format!("[{},{},{},{}]", json_str(s), json_str(p), a, d)).collect();
    parts.push(("files", format!("[{}]", frows.join(","))));
    if !conflicts.is_empty() {
        let mut cs = conflicts.clone();
        cs.sort();
        let arr: Vec<String> = cs.iter().map(|c| json_str(c)).collect();
        parts.push(("conflicts", format!("[{}]", arr.join(","))));
    }
    if ahead != 0 || behind != 0 {
        parts.push(("ahead", ahead.to_string()));
        parts.push(("behind", behind.to_string()));
    }
    parts.sort_by(|a, b| a.0.cmp(b.0));
    let body: Vec<String> = parts.iter().map(|(k, v)| format!("\"{}\":{}", k, v)).collect();
    println!("{{{}}}", body.join(","));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("st") => cmd_st(),
        Some("version") | Some("--version") => println!("{{\"core\":\"keel-core\",\"keel\":\"0.1.0-rust\"}}"),
        Some(other) => die("E_USAGE", &format!("not-yet-ported command: {}", other), "Rust CLI v0 ports st; keel.mjs for the rest"),
        None => println!("keel (rust) — st, version. Full surface on keel.mjs during the port."),
    }
}
