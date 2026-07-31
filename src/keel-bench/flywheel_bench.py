#!/usr/bin/env python3
"""
Live retrieval-lift benchmark for keel's flywheel (Rust keel) — larger + repeated.

Claim under test: an agent is MORE CORRECT when keel surfaces the non-obvious prior lesson for
the task. Each scenario carries an ARBITRARY project convention a competent model can't guess a
priori. The convention is recorded in a real keel repo via `keel learn`; `keel brief` retrieves
it. We solve each task WITHOUT the brief and WITH it, T trials each, and a dual LLM judge scores
rule-compliance. Lift = WITH% - WITHOUT%. Reports Wilson 95% CIs.

Env: TRIALS (default 3), WORKERS (default 6). Reads the API key from ~/.claude-token.
"""
import json, math, os, subprocess, tempfile, urllib.request, urllib.error, pathlib, time, random
from concurrent.futures import ThreadPoolExecutor, as_completed

KEEL = os.environ.get("KEEL_BIN", str(pathlib.Path.home() / "keel/src/rust/target/release/keel"))
SOLVER = "claude-opus-4-8"      # the agent under test — must be capable
JUDGE = "claude-sonnet-5"       # rule-compliance yes/no
API = "https://api.anthropic.com/v1/messages"
KEY = (pathlib.Path.home() / ".claude-token").read_text().strip()
TRIALS = int(os.environ.get("TRIALS", "3"))
WORKERS = int(os.environ.get("WORKERS", "6"))

def api(system, user, model, max_tokens=1600):
    body = json.dumps({"model": model, "max_tokens": max_tokens, "system": system,
                       "messages": [{"role": "user", "content": user}]}).encode()
    for attempt in range(6):
        req = urllib.request.Request(API, data=body, headers={
            "x-api-key": KEY, "anthropic-version": "2023-06-01", "content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                d = json.load(r)
            return "".join(b.get("text", "") for b in d.get("content", []) if b.get("type") == "text")
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 529) and attempt < 5:
                time.sleep((2 ** attempt) + random.random()); continue
            raise
        except Exception:
            if attempt < 5:
                time.sleep((2 ** attempt) + random.random()); continue
            raise
    return ""

# (file, starter, task, arbitrary rule/lesson, judge compliance-criterion)
SCEN = [
 ("logger.js","export function logStart(job){\n  // TODO\n}","Implement logStart(job): record that a job started.",
  "All logging goes through audit(event, {traceId}); console.log is forbidden.",
  "calls audit(...) with a traceId and does NOT use console.log"),
 ("event.js","export function stamp(evt){\n  // TODO\n}","Implement stamp(evt): set evt.at to the current time.",
  "Timestamps are stored as integer utcMillis(); never Date, Date.now(), or ISO strings.",
  "sets evt.at from utcMillis() and does NOT use Date, Date.now, or an ISO string"),
 ("settings.js","export function port(){\n  // TODO\n}","Implement port(): return the configured HTTP port.",
  "Config is read only via cfg.get(key, {required:true}); direct process.env access is forbidden.",
  "reads via cfg.get(...) and does NOT touch process.env"),
 ("handler.js","export async function getOrders(ctx){\n  // TODO\n}","Implement getOrders(ctx): return the caller's orders.",
  "Every request handler must call assertTenant(ctx) as its first statement.",
  "assertTenant(ctx) is the FIRST statement in the function body"),
 ("cache.js","export function value(cache, key){\n  // TODO\n}","Implement value(cache, key): read a value from the cache.",
  "Call refresh() before any read() on the cache, or it serves stale data.",
  "calls refresh() before it calls read()"),
 ("net.js","export async function fetchQuote(url){\n  // TODO\n}","Implement fetchQuote(url): fetch with retries.",
  "Retries use withBackoff(fn, {jitter:true}); a hand-written retry loop is forbidden.",
  "uses withBackoff(..., {jitter:true}) and has NO manual for/while retry loop"),
 ("price.js","export function total(items){\n  // TODO\n}","Implement total(items): sum item prices for display.",
  "Money is integer Cents; format with fmtMoney(cents). Never use floats or toFixed.",
  "sums integer cents and formats via fmtMoney; NO floating point or toFixed"),
 ("errors.js","export function must(cond){\n  // TODO\n}","Implement must(cond): throw if cond is false.",
  "Throw AppError(code, msg) with a code from codes.ts; never `throw new Error`.",
  "throws AppError(code, msg); does NOT use `throw new Error`"),
 ("ids.js","export function make(){\n  // TODO\n}","Implement make(): return a new order id.",
  "Generate ids with newId('prefix') (ULID); never Math.random or a uuid library.",
  "returns newId('...'); does NOT use Math.random or uuid"),
 ("ui.js","export function greeting(name){\n  // TODO\n}","Implement greeting(name): return a user-facing greeting.",
  "User-facing strings must be t('key', vars); raw string literals shown to users are forbidden.",
  "builds the greeting via t('...'); NO raw user-facing string literal"),
 ("page.js","export async function listUsers(q){\n  // TODO\n}","Implement listUsers(q): list users, paginated.",
  "List endpoints paginate with cursorPage(query, cursor); offset/limit is forbidden.",
  "uses cursorPage(...); does NOT use offset or limit pagination"),
 ("fmt.js","export function showDate(ms, tz){\n  // TODO\n}","Implement showDate(ms, tz): format a date for display.",
  "Format display dates with fmtDate(ms, tz); never toLocaleString or manual concatenation.",
  "uses fmtDate(ms, tz); NO toLocaleString or manual string concatenation"),
 ("secret.js","export function apiKey(){\n  // TODO\n}","Implement apiKey(): return the third-party API key.",
  "Secrets are read via vault.get(name); never from env vars or config files.",
  "reads via vault.get(...); does NOT read env or a config file"),
 ("db.js","export async function userByEmail(email){\n  // TODO\n}","Implement userByEmail(email): query a user by email.",
  "Queries use the sql`...` tagged template (parameterized); never string concatenation.",
  "uses a sql`...` tagged template; does NOT concatenate the email into the query string"),
 ("flag.js","export function newUI(ctx){\n  // TODO\n}","Implement newUI(ctx): whether to show the new UI.",
  "Gate features with flag('name', ctx); never read a boolean straight from config.",
  "uses flag('...', ctx); does NOT read a boolean directly from config"),
 ("bus.js","export function orderPlaced(order){\n  // TODO\n}","Implement orderPlaced(order): announce an order was placed.",
  "Emit domain events via bus.publish(Event.of(...)); never call handlers directly.",
  "calls bus.publish(Event.of(...)); does NOT invoke a handler function directly"),
 ("valid.js","export function parseSignup(input){\n  // TODO\n}","Implement parseSignup(input): validate signup input.",
  "Validate input with SignupSchema.parse(input); never hand-written if-checks.",
  "validates via a Schema.parse(input) call; NO manual if/throw field checks"),
 ("resp.js","export function getItem(ctx, id){\n  // TODO\n}","Implement getItem(ctx, id): return an item or a not-found response.",
  "Return responses via ok(data) / fail(code); never res.status().json() directly.",
  "returns via ok(...) / fail(...); does NOT call res.status(...).json(...)"),
 ("lock.js","export async function renumber(key){\n  // TODO\n}","Implement renumber(key): run a critical section for key.",
  "Guard critical sections with withLock(key, fn); never a boolean mutex flag.",
  "wraps the work in withLock(key, ...); does NOT use a boolean lock flag"),
 ("metric.js","export function onSignup(){\n  // TODO\n}","Implement onSignup(): record a signup happened.",
  "Record metrics via metric.inc('name', tags); never console or a global counter.",
  "calls metric.inc('...', ...); does NOT use console or a global counter variable"),
]

def sh(a, cwd=None): return subprocess.run([KEEL] + a, cwd=cwd, capture_output=True, text=True)

def setup(repo):
    for u in (["config","user.email","b@e.com"],["config","user.name","bench"]):
        subprocess.run(["git","-C",repo]+u, capture_output=True)
    sh(["init"], cwd=repo)
    for u in (["config","user.email","b@e.com"],["config","user.name","bench"]):
        subprocess.run(["git","-C",repo]+u, capture_output=True)
    for fname, starter, _t, lesson, _c in SCEN:
        pathlib.Path(repo, fname).write_text(starter + "\n")
        sh(["add","-A"], cwd=repo); sh(["commit","-m",f"add {fname}"], cwd=repo)
        sh(["learn","--task",fname,"--lesson",lesson], cwd=repo)

def retrieved(repo, fname):
    r = sh(["brief","--file",fname,"--json"], cwd=repo)
    try:
        s = json.loads(r.stdout).get("sessions", [])
        return s[0]["lesson"] if s else None
    except Exception:
        return None

def solve(fname, starter, task, lesson):
    sysmsg = "You are a careful senior engineer in an existing codebase. Output ONLY the function code — no prose, no markdown fences."
    user = f"File `{fname}`:\n```\n{starter}\n```\nTask: {task}"
    if lesson:
        user += f"\n\nRetrieved from this codebase's history (a prior lesson that applies here):\n\"{lesson}\""
    return solve_code(sysmsg, user)

def solve_code(sysmsg, user):
    return api(sysmsg, user, SOLVER, max_tokens=1800)

def judge(fname, code, crit):
    q = (f"Compliance check: the code {crit}.\n\nCode for `{fname}`:\n```\n{code}\n```\n\n"
         "Does the code satisfy the check? Answer with exactly one word: YES or NO.")
    votes = 0
    for _ in range(2):
        a = api("You are a strict code reviewer. Judge only the stated compliance check.", q, JUDGE, max_tokens=600).strip().upper()
        toks = a.split()
        if (toks and toks[-1].startswith("YES")) or a.startswith("YES"):
            votes += 1
    return votes >= 2

def trial(item):
    (si, cond, _t) = item
    fname, starter, task, lesson, crit, got = si
    code = solve(fname, starter, task, (got or lesson) if cond == "with" else None)
    return (fname, cond, judge(fname, code, crit))

def wilson(k, n, z=1.96):
    if n == 0: return (0.0, 0.0)
    p = k / n; d = 1 + z*z/n
    c = (p + z*z/(2*n)) / d
    h = (z * math.sqrt(p*(1-p)/n + z*z/(4*n*n))) / d
    return (max(0, c-h), min(1, c+h))

def main():
    repo = tempfile.mkdtemp(prefix="keel-fly-")
    setup(repo)
    # attach retrieved lessons up front, and record retrieval success
    scen, retr_ok = [], 0
    for s in SCEN:
        got = retrieved(repo, s[0]); retr_ok += 1 if (got and s[3][:20] in got) else 0
        scen.append(s + (got,))
    items = [(si, cond, t) for si in scen for cond in ("without","with") for t in range(TRIALS)]
    print(f"scenarios={len(SCEN)} trials={TRIALS} solver={SOLVER} judge={JUDGE} · {len(items)} solves in flight…")
    results = {}
    with ThreadPoolExecutor(max_workers=WORKERS) as ex:
        futs = {ex.submit(trial, it): it for it in items}
        for f in as_completed(futs):
            fname, cond, ok = f.result()
            results.setdefault((fname, cond), []).append(ok)
    n = len(SCEN)
    print(f"\n{'scenario':<12} {'WITHOUT':>10} {'WITH':>10}")
    print("-"*36)
    wo = wi = 0
    for s in SCEN:
        rw = results.get((s[0],"without"),[]); ri = results.get((s[0],"with"),[])
        wo += sum(rw); wi += sum(ri)
        print(f"{s[0]:<12} {f'{sum(rw)}/{len(rw)}':>10} {f'{sum(ri)}/{len(ri)}':>10}")
    tot = n*TRIALS
    lw, hw = wilson(wo, tot); li, hi = wilson(wi, tot)
    print("-"*36)
    print(f"lessons retrieved by keel brief: {retr_ok}/{n}")
    print(f"WITHOUT keel brief: {wo}/{tot} = {100*wo/tot:.0f}%   (95% CI {100*lw:.0f}–{100*hw:.0f}%)")
    print(f"WITH    keel brief: {wi}/{tot} = {100*wi/tot:.0f}%   (95% CI {100*li:.0f}–{100*hi:.0f}%)")
    print(f"LIFT: +{100*(wi-wo)/tot:.0f} points")
    import shutil; shutil.rmtree(repo, ignore_errors=True)

if __name__ == "__main__":
    main()
