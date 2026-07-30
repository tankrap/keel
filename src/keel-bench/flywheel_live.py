#!/usr/bin/env python3
"""Live flywheel eval (NEW-1105/1091/1106): does keel's ACTUAL retrieval lift agent correctness?

For each scenario we (1) commit a file into a real keel repo with a captured session whose
`lesson` is a project-specific CONVENTION (before/after/instead-of/wrap — not encoded in any
type signature, so a model cannot infer it from the code); (2) ask keel for a brief and pull
the lesson it *actually retrieves*; (3) have an LLM agent do a related task WITHOUT and WITH
that retrieved lesson; (4) grade each with TWO independent judges (pass = both agree), to cut
single-judge noise. The measured lift is WITH-correct minus WITHOUT-correct.

Honest scope: lessons are authored (no public corpus of real agent sessions to draw from),
but the code, keel's retrieval, the agent, and dual-judge grading are all real. N and judge
robustness are scaled up from the n=4 single-judge first cut. Keys read from ~/.claude-token,
never printed. Usage: python3 flywheel_live.py
"""
import json, os, subprocess, tempfile, shutil, pathlib, urllib.request

KEEL = os.path.expanduser("~/keel/src/rust/target/release/keel")
KEY = pathlib.Path.home().joinpath(".claude-token").read_text().strip()
MODEL = "claude-opus-4-8"
TMP = os.path.expanduser("~/.claude/jobs/dad320ca/tmp")

AGENT_SYS = (
    "You are a senior engineer on an existing codebase. Use only the primitives shown. "
    "Output ONLY the code for the requested function — no prose, no markdown fences."
)

# Convention rules (not type-encoded) → a model can't infer them from the primitives.
SCENARIOS = [
    {"name": "payments/reconcile-before-post", "file": "pay.ts",
     "code": "export const gateway = { post: (amt: number) => 0 };\nexport function reconcile(): void {}\n",
     "lesson": "You MUST call reconcile() immediately before gateway.post(), or the ledger double-posts.",
     "task": "Add `export function payout(amt: number)` that sends a payment via gateway.post.",
     "rubric": "calls reconcile() before it calls gateway.post()"},
    {"name": "auth/ed25519-not-hmac", "file": "auth.ts",
     "code": "export function verifyEd25519(t: string): boolean { return t.length > 0; }\nexport function verifyHmac(t: string): boolean { return true; }\n",
     "lesson": "Authenticate with verifyEd25519. verifyHmac is legacy and returns true unconditionally in prod — never use it.",
     "task": "Add `export function requireAuth(t: string): boolean` that authenticates a token.",
     "rubric": "uses verifyEd25519 and does NOT use verifyHmac"},
    {"name": "cache/evict-after-write", "file": "users.ts",
     "code": "export const store = { save: (id: string, n: string) => {} };\nexport const cache = { evict: (id: string) => {} };\n",
     "lesson": "After any write to a user you MUST call cache.evict(id), or stale reads persist for 24h.",
     "task": "Add `export function rename(id: string, name: string)` that saves the new name.",
     "rubric": "calls cache.evict(id) after store.save"},
    {"name": "http/wrap-withRetry", "file": "client.ts",
     "code": "export const http = { get: (u: string) => ({} as any) };\nexport function withRetry<T>(f: () => T): T { return f(); }\n",
     "lesson": "Every external http.get MUST be wrapped in withRetry(() => ...); the upstream 429s aggressively.",
     "task": "Add `export function fetchProfile(u: string)` that GETs a profile URL.",
     "rubric": "wraps the http.get call inside withRetry(...)"},
    {"name": "events/publish-after-create", "file": "orders.ts",
     "code": "export const db = { insert: (o: any) => 1 };\nexport const bus = { publish: (ev: string, p: any) => {} };\n",
     "lesson": "After creating an order you MUST bus.publish('orderCreated', order); billing/email depend on it.",
     "task": "Add `export function createOrder(o: any)` that inserts an order.",
     "rubric": "calls bus.publish('orderCreated', ...) after db.insert"},
    {"name": "logging/redact-before-log", "file": "log.ts",
     "code": "export const logger = { info: (m: any) => {} };\nexport function redact(u: any): any { return u; }\n",
     "lesson": "Never log a user object directly — pass it through redact() first (PII policy).",
     "task": "Add `export function logUser(u: any)` that logs a user at info level.",
     "rubric": "calls logger.info with redact(u), not the raw user"},
    {"name": "money/fromCents-not-float", "file": "money.ts",
     "code": "export class Money { static fromCents(c: number): Money { return new Money(); } }\n",
     "lesson": "Monetary amounts are integer cents — construct with Money.fromCents(n); never use a raw/float number.",
     "task": "Add `export function priceOf(cents: number)` that returns a Money for a price.",
     "rubric": "constructs the value via Money.fromCents"},
    {"name": "locks/ordered-acquire", "file": "lock.ts",
     "code": "export const lockManager = { acquireOrdered: (ids: string[]) => {}, acquire: (id: string) => {} };\n",
     "lesson": "Acquire multiple locks only via lockManager.acquireOrdered(ids) — per-id acquire() in a loop deadlocks.",
     "task": "Add `export function transfer(a: string, b: string)` that locks both accounts.",
     "rubric": "uses lockManager.acquireOrdered with both ids, not a per-id acquire loop"},
    {"name": "flags/gate-endpoint", "file": "api.ts",
     "code": "export const flags = { assertEnabled: (k: string) => {} };\n",
     "lesson": "Every new endpoint handler must call flags.assertEnabled('<name>') as its first line (staged rollout policy).",
     "task": "Add `export function handleExport()` — a new endpoint handler.",
     "rubric": "calls flags.assertEnabled(...) at the start"},
    {"name": "fs/sanitize-before-access", "file": "files.ts",
     "code": "export const fs2 = { read: (p: string) => '' };\nexport function sanitizePath(p: string): string { return p; }\n",
     "lesson": "User-supplied paths MUST go through sanitizePath() before any fs2.read (path-traversal policy).",
     "task": "Add `export function readUserFile(p: string)` that reads a user-provided path.",
     "rubric": "calls sanitizePath(p) before fs2.read"},
]


def sh(args):
    return subprocess.run(args, capture_output=True, text=True)


def llm(system, user, max_tokens=600):
    body = json.dumps({"model": MODEL, "max_tokens": max_tokens, "system": system,
                       "messages": [{"role": "user", "content": user}]}).encode()
    req = urllib.request.Request("https://api.anthropic.com/v1/messages", data=body,
        headers={"x-api-key": KEY, "anthropic-version": "2023-06-01", "content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=180) as r:
        d = json.load(r)
    return "".join(b.get("text", "") for b in d.get("content", []) if b.get("type") == "text")


def judges_pass(rubric, sol):
    # two independently-framed judges; PASS only if both agree (conservative → fewer false positives)
    a = llm("You are a strict reviewer. First line exactly PASS or FAIL.",
            f"Requirement: the code {rubric}.\n\nCode:\n{sol}\n\nDoes it meet the requirement?", 16)
    b = llm("You verify code against a rule. Reply exactly YES or NO on the first line.",
            f"Rule: the code must {rubric}.\n\nCode:\n{sol}\n\nDoes the code follow the rule?", 16)
    pa = a.strip().upper().startswith("PASS")
    pb = b.strip().upper().startswith("YES")
    return pa and pb


def setup(sc):
    d = tempfile.mkdtemp(prefix="keel-fw-", dir=TMP)
    pathlib.Path(d, sc["file"]).write_text(sc["code"])
    sess = pathlib.Path(d, "_s.json")
    sess.write_text(json.dumps({"task": f"implement {sc['file']}", "model": "prior",
                                "lesson": sc["lesson"], "verification": "green"}))
    sh([KEEL, "init", "--root", d])
    sh([KEEL, "commit", "--root", d, "-m", "prior", "--session", str(sess)])
    return d


def retrieved(d, sc):
    r = sh([KEEL, "brief", "--root", d, "--file", sc["file"], "--json"])
    try:
        b = json.loads(r.stdout)
    except Exception:
        return ""
    parts = [s.get("lesson", "") for s in b.get("sessions", [])] + [i.get("lesson", "") for i in b.get("invariants", [])]
    return " ".join(p for p in parts if p)


def run():
    n = len(SCENARIOS)
    wo = w = rok = 0
    print(f"flywheel live eval — n={n}, dual-judge, model={MODEL}\n")
    print(f"{'scenario':<34}{'retr':>5}{'WITHOUT':>9}{'WITH':>6}")
    print("-" * 54)
    for sc in SCENARIOS:
        d = setup(sc)
        try:
            lesson = retrieved(d, sc)
            rok += 1 if lesson else 0
            prompt = f"Task: {sc['task']}\n\nExisting file {sc['file']}:\n{sc['code']}\n"
            g_wo = judges_pass(sc["rubric"], llm(AGENT_SYS, prompt))
            ctx = f"Relevant prior knowledge retrieved by keel:\n- {lesson}\n\n" if lesson else ""
            g_w = judges_pass(sc["rubric"], llm(AGENT_SYS, prompt + "\n" + ctx))
            wo += g_wo
            w += g_w
            print(f"{sc['name']:<34}{'yes' if lesson else 'NO':>5}{('PASS' if g_wo else 'FAIL'):>9}{('PASS' if g_w else 'FAIL'):>6}")
        finally:
            shutil.rmtree(d, ignore_errors=True)
    print("-" * 54)
    print(f"retrieval surfaced the lesson : {rok}/{n}")
    print(f"WITHOUT keel : {wo}/{n} correct ({100*wo//n}%)")
    print(f"WITH keel    : {w}/{n} correct ({100*w//n}%)")
    print(f"flywheel lift: +{w - wo}/{n}  (+{100*(w-wo)//n} pts)")


if __name__ == "__main__":
    run()
