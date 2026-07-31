#!/usr/bin/env python3
"""
Live retrieval-lift benchmark for keel's flywheel (Rust keel).

Claim under test: an agent is MORE CORRECT when keel surfaces the non-obvious prior lesson
for the task. Each scenario carries an ARBITRARY project convention a competent model can't
guess a priori (audit()/utcMillis()/assertTenant()/…). The convention is recorded in a real
keel repo via `keel learn`; `keel brief` retrieves it. We solve each task WITHOUT the brief
and WITH it, and a dual LLM judge scores rule-compliance. Lift = WITH% - WITHOUT%.

Usage: python3 flywheel_bench.py   (reads the API key from ~/.claude-token)
"""
import json, os, subprocess, sys, tempfile, urllib.request, pathlib

KEEL = os.environ.get("KEEL_BIN", str(pathlib.Path.home() / "keel/src/rust/target/release/keel"))
MODEL = "claude-opus-4-8"
API = "https://api.anthropic.com/v1/messages"
KEY = (pathlib.Path.home() / ".claude-token").read_text().strip()

def api(system, user, max_tokens=2000):
    # opus-4.8 uses adaptive thinking by default (temperature is forced to 1; thinking blocks need
    # token headroom). We read only the final text block, skipping any thinking.
    body = json.dumps({"model": MODEL, "max_tokens": max_tokens,
                       "system": system, "messages": [{"role": "user", "content": user}]}).encode()
    req = urllib.request.Request(API, data=body, headers={
        "x-api-key": KEY, "anthropic-version": "2023-06-01", "content-type": "application/json"})
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                d = json.load(r)
            return "".join(b.get("text", "") for b in d.get("content", []))
        except Exception as e:
            if attempt == 2:
                raise
    return ""

# (file, starter, task, lesson/rule, judge-criterion)
SCEN = [
 ("logger.js", "export function logStart(job){\n  // TODO\n}",
  "Implement logStart(job): record that a job started.",
  "All logging goes through audit(event, {traceId}); console.log is forbidden.",
  "The code calls audit(...) with a traceId and does NOT use console.log."),
 ("event.js", "export function stamp(evt){\n  // TODO: set evt.at\n}",
  "Implement stamp(evt): set evt.at to the current time.",
  "Timestamps are stored as integer utcMillis(); never Date, Date.now(), or ISO strings.",
  "evt.at is set from utcMillis(); the code does NOT use Date, Date.now, or an ISO string."),
 ("settings.js", "export function port(){\n  // TODO\n}",
  "Implement port(): return the configured HTTP port.",
  "Config is read only via cfg.get(key, {required:true}); direct process.env access is forbidden.",
  "The code reads the port via cfg.get(...) and does NOT touch process.env."),
 ("handler.js", "export async function getOrders(ctx){\n  // TODO\n}",
  "Implement getOrders(ctx): return the caller's orders.",
  "Every request handler must call assertTenant(ctx) as its first statement or data leaks across tenants.",
  "assertTenant(ctx) is the FIRST statement in the function body."),
 ("cache.js", "export function value(cache, key){\n  // TODO\n}",
  "Implement value(cache, key): read a value from the cache.",
  "Call refresh() before any read() on the cache, or it serves stale data.",
  "The code calls refresh() before it calls read()."),
 ("net.js", "export async function fetchQuote(url){\n  // TODO\n}",
  "Implement fetchQuote(url): fetch with retries.",
  "Retries use withBackoff(fn, {jitter:true}); a hand-written retry loop is forbidden.",
  "The code uses withBackoff(..., {jitter:true}) and has NO manual for/while retry loop."),
 ("price.js", "export function total(items){\n  // TODO\n}",
  "Implement total(items): sum item prices for display.",
  "Money is integer Cents; format with fmtMoney(cents). Never use floats or toFixed.",
  "The code sums integer cents and formats via fmtMoney; NO floating point or toFixed."),
 ("errors.js", "export function must(cond){\n  // TODO\n}",
  "Implement must(cond): throw if cond is false.",
  "Throw AppError(code, msg) with a code from codes.ts; never `throw new Error`.",
  "The code throws AppError(code, msg); it does NOT use `throw new Error`."),
 ("ids.js", "export function make(){\n  // TODO\n}",
  "Implement make(): return a new order id.",
  "Generate ids with newId('prefix') (ULID); never Math.random or a uuid library.",
  "The code returns newId('...'); it does NOT use Math.random or uuid."),
 ("ui.js", "export function greeting(name){\n  // TODO\n}",
  "Implement greeting(name): return a user-facing greeting.",
  "User-facing strings must be t('key', vars); raw string literals shown to users are forbidden.",
  "The greeting is built via t('...'); there is NO raw user-facing string literal."),
]

def sh(args, cwd=None):
    return subprocess.run([KEEL] + args, cwd=cwd, capture_output=True, text=True)

def setup(repo):
    subprocess.run(["git", "-C", repo, "config", "user.email", "b@e.com"], capture_output=True)
    subprocess.run(["git", "-C", repo, "config", "user.name", "bench"], capture_output=True)
    sh(["init"], cwd=repo)
    subprocess.run(["git", "-C", repo, "config", "user.email", "b@e.com"], capture_output=True)
    subprocess.run(["git", "-C", repo, "config", "user.name", "bench"], capture_output=True)
    for fname, starter, _task, lesson, _chk in SCEN:
        pathlib.Path(repo, fname).write_text(starter + "\n")
        sh(["add", "-A"], cwd=repo)
        sh(["commit", "-m", f"add {fname}"], cwd=repo)
        sh(["learn", "--task", fname, "--lesson", lesson], cwd=repo)

def retrieved_lesson(repo, fname):
    r = sh(["brief", "--file", fname, "--json"], cwd=repo)
    try:
        d = json.loads(r.stdout)
        s = d.get("sessions", [])
        return s[0]["lesson"] if s else None
    except Exception:
        return None

def solve(starter, fname, task, lesson):
    sysmsg = "You are a careful senior engineer working in an existing codebase. Output ONLY the function code, no prose, no markdown fences."
    user = f"File `{fname}`:\n```\n{starter}\n```\nTask: {task}"
    if lesson:
        user += f"\n\nRetrieved from this codebase's history (a prior lesson that applies here):\n\"{lesson}\""
    return api(sysmsg, user)

def judge(fname, code, criterion):
    votes = 0
    q = (f"A codebase has this rule. Rule-compliance check: {criterion}\n\n"
         f"Code submitted for `{fname}`:\n```\n{code}\n```\n\n"
         "Does the code satisfy the compliance check? Reply with exactly one word: YES or NO.")
    for _ in range(2):
        a = api("You are a strict code reviewer. Judge only the stated compliance check.", q, max_tokens=1024).strip().upper()
        tail = a.split()[-1] if a.split() else ""
        if tail.startswith("YES") or a.startswith("YES"):
            votes += 1
    return votes >= 2  # both judges must agree it complies

def main():
    repo = tempfile.mkdtemp(prefix="keel-fly-")
    setup(repo)
    print(f"{'scenario':<12} {'lesson retrieved':>16}  {'WITHOUT':>8}  {'WITH':>6}")
    print("-" * 50)
    wo_ok = wi_ok = 0
    for fname, starter, task, lesson, crit in SCEN:
        got = retrieved_lesson(repo, fname)
        retrieved = got is not None and lesson[:20] in got
        code_wo = solve(starter, fname, task, None)
        code_wi = solve(starter, fname, task, got or lesson)
        p_wo = judge(fname, code_wo, crit)
        p_wi = judge(fname, code_wi, crit)
        wo_ok += p_wo; wi_ok += p_wi
        print(f"{fname:<12} {('yes' if retrieved else 'NO'):>16}  {('PASS' if p_wo else 'fail'):>8}  {('PASS' if p_wi else 'fail'):>6}")
    n = len(SCEN)
    print("-" * 50)
    print(f"WITHOUT keel brief : {wo_ok}/{n}  ({100*wo_ok//n}%)")
    print(f"WITH    keel brief : {wi_ok}/{n}  ({100*wi_ok//n}%)")
    print(f"LIFT               : +{100*wi_ok//n - 100*wo_ok//n} points")
    import shutil; shutil.rmtree(repo, ignore_errors=True)

if __name__ == "__main__":
    main()
