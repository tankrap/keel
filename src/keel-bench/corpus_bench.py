#!/usr/bin/env python3
"""
Real-corpus flywheel benchmark — VS Code.

The synthetic flywheel run invited one objection: "you INVENTED those conventions, of course
retrieval helps." This run answers it. Every convention below is a real, documented VS Code rule,
grounded by its frequency in the actual source (measured on a fresh microsoft/vscode checkout):
  undefined-not-null   66,458 `undefined` vs 0 `: null` type usages
  this._register(...)   12,787 occurrences        @IXxxService injection  15,401
  Emitter/onDid          3,517 / 3,806            URI.                    12,242
  CancellationToken      6,265                    localize(               21,381
  registerSingleton        677                    assertNever                44
Each convention is attached (via `keel learn`) to the REAL VS Code file it governs, committed into
a keel repo with its real path + real content. `keel brief --file <real file>` retrieves it. We
solve a task set in that module WITHOUT the brief and WITH it, T trials each; a strict dual judge
(Sonnet 5) scores compliance with the real rule. Solver = Opus 4.8 (the agent under test).

Env: CORPUS_SRC (a vscode checkout, default ~/keel-vscode-demo), TRIALS (3), WORKERS (6).
"""
import json, os, subprocess, tempfile, shutil, pathlib

from bench_common import (api, judge, sh, wilson, run_trials, build_summary,
                          SOLVER, JUDGE, KEEL, TRIALS, WORKERS)

CORPUS = os.environ.get("CORPUS_SRC", str(pathlib.Path.home() / "keel-vscode-demo"))

# (real vscode file, module hint, task, real grounded convention, judge compliance-criterion)
SCEN = [
 ("src/vs/base/common/lifecycle.ts", "the disposables/lifecycle module",
  "Implement a class PollingWatcher that starts a setInterval in its constructor and must clean it up when disposed.",
  "Own resources by extending Disposable and registering each with this._register(...); never keep a raw handle you clear by hand.",
  "extends Disposable (or uses a DisposableStore) and registers the interval via this._register/toDisposable, not a bare timer id it clears manually"),
 ("src/vs/base/common/uri.ts", "the URI module",
  "Implement childResource(parent, name) that returns the resource for a child entry of a folder.",
  "Resources are URI, never raw path strings; derive child paths with URI helpers (URI.joinPath/.with), not string concatenation or node path.",
  "takes and returns a URI and joins via URI helpers (e.g. URI.joinPath), not string concatenation of a path"),
 ("src/vs/base/common/event.ts", "the event/Emitter module",
  "Add a change-notification event to a Model class whose setValue(v) should notify listeners.",
  "Expose events as `readonly onDidChange = this._register(new Emitter<T>()).event` and fire through the private emitter; never keep an array of callbacks.",
  "exposes the event via an Emitter's .event and fires with emitter.fire(), rather than maintaining a list of callbacks"),
 ("src/vs/base/common/cancellation.ts", "the cancellation module",
  "Implement async processAll(items, token) that processes each item in turn.",
  "Long-running async work takes a CancellationToken and checks token.isCancellationRequested (or raceCancellation); never an uncancellable loop.",
  "accepts a CancellationToken parameter and checks isCancellationRequested to bail out during the work"),
 ("src/vs/base/common/types.ts", "the type-guards module",
  "Implement area(shape) that switches over a Shape union of {kind:'circle',r} | {kind:'square',s}.",
  "Close an exhaustive switch over a union with `default: assertNever(x)` so adding a case becomes a compile error.",
  "ends the switch with a default branch that calls assertNever (an exhaustiveness check), not a plain default/throw of a string"),
 ("src/vs/base/common/map.ts", "the collections module",
  "Implement lastOf(items) that returns the last element of an array, or nothing if empty.",
  "Absent values are always `undefined`, never `null`; type optionals as `T | undefined` and return undefined.",
  "returns undefined for the empty case and never uses null"),
 ("src/vs/platform/instantiation/common/instantiation.ts", "the dependency-injection module",
  "Write the constructor of a class MyController that needs logging and configuration.",
  "Depend on services via constructor parameter decorators, e.g. `@ILogService private readonly logService: ILogService`; never new-up a concrete service.",
  "declares its dependencies as @IXxxService-decorated constructor parameters, not by constructing concrete services"),
 ("src/vs/platform/instantiation/common/extensions.ts", "the service-registration module",
  "Register a new implementation ClipboardService of IClipboardService so the DI container can create it.",
  "Register service implementations with registerSingleton(IFooService, FooService, InstantiationType.Delayed); never export a shared instance.",
  "uses registerSingleton(IService, Impl, ...) rather than exporting a module-level singleton instance"),
 ("src/vs/platform/log/common/log.ts", "the logging module",
  "Add a diagnostic line to a Connection class when a connection opens.",
  "Diagnostics go through the injected ILogService (logService.info/warn/error); console.log is banned in product code.",
  "logs via an injected logService.* method, not console.log"),
 ("src/vs/base/common/async.ts", "the async-helpers module",
  "Coalesce rapid save() calls so that after a burst only one save runs, once things go quiet for 500ms.",
  "Debounce/throttle via the shared helpers in async.ts (Delayer/Throttler); never hand-roll setTimeout-based control flow.",
  "uses a Delayer/Throttler-style helper (schedule with a delay) rather than a hand-written setTimeout debounce"),
 ("src/vs/base/common/arrays.ts", "the array-helpers module",
  "Given an array of type (T | undefined)[], return a T[] with the missing entries removed.",
  "Drop nullish entries with the codebase helper coalesce(...); don't hand-roll filter(Boolean) (it mistypes and drops falsy values).",
  "uses coalesce(...) (the codebase helper) rather than a hand-written filter(Boolean) / filter(x=>x)"),
 ("src/vs/base/common/errors.ts", "the errors module",
  "In a catch block, rethrow if the failure was a cancellation, otherwise report it.",
  "Detect cancellation with isCancellationError(e) and report other failures via onUnexpectedError(e); never silently swallow.",
  "uses isCancellationError to detect cancellation and onUnexpectedError to report the rest"),
 ("src/vs/base/common/strings.ts", "the strings module",
  "Case-insensitively compare two label strings for equality.",
  "Compare strings with the helpers in strings.ts (e.g. equalsIgnoreCase); avoid ad-hoc a.toLowerCase() === b.toLowerCase().",
  "uses equalsIgnoreCase (the codebase helper) rather than comparing a.toLowerCase() === b.toLowerCase()"),
 ("src/vs/nls.ts", "the localization (nls) module",
  "Return the tooltip text for a 'Format Document' toolbar action.",
  "User-facing strings must go through localize('key', \"Text\") from vs/nls; never a raw string literal shown to the user.",
  "wraps the user-facing text in localize('...', ...), not a raw string literal"),
 ("src/vs/base/common/resources.ts", "the resource-helpers module",
  "Determine whether two resources refer to the same file.",
  "Compare/manipulate URIs with the resources.ts helpers (isEqual, dirname, joinPath); never compare uri.toString() or split fsPath by hand.",
  "uses isEqual(a, b) (or extUri.isEqual) rather than comparing uri.toString() or fsPath strings"),
 ("src/vs/platform/configuration/common/configuration.ts", "the configuration module",
  "Read the user's configured editor tab size.",
  "Read settings via IConfigurationService.getValue('section.key'); never process.env or a global for configuration.",
  "reads via configurationService.getValue('editor.tabSize'), not env or a hardcoded global"),
]

def build_and_retrieve(path, lesson):
    """Give each REAL vscode file its own keel repo, commit it (real path + real content), attach
    its real convention via `keel learn`, and retrieve it with `keel brief`. One file per repo so
    retrieval is clean — brief also surfaces dependency-graph NEIGHBOURS' lessons, so these
    heavily cross-importing base modules would otherwise contaminate each other's results."""
    repo = tempfile.mkdtemp(prefix="keel-corpus-")
    for u in (["config","user.email","b@e.com"],["config","user.name","bench"]):
        subprocess.run(["git","-C",repo]+u, capture_output=True)
    sh(["init"], cwd=repo)
    for u in (["config","user.email","b@e.com"],["config","user.name","bench"]):
        subprocess.run(["git","-C",repo]+u, capture_output=True)
    dst = pathlib.Path(repo, path)
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(pathlib.Path(CORPUS, path), dst)    # real vscode content at its real path
    sh(["add","-A"], cwd=repo)
    sh(["commit","-m",f"add {path}"], cwd=repo)
    sh(["learn","--task",path,"--lesson",lesson], cwd=repo)
    r = sh(["brief","--file",path,"--json"], cwd=repo)
    shutil.rmtree(repo, ignore_errors=True)
    try:
        s = json.loads(r.stdout).get("sessions", [])
        return s[0]["lesson"] if s else None
    except Exception:
        return None

def solve(path, hint, task, lesson):
    sysmsg = ("You are a senior engineer working in the VS Code codebase (TypeScript). Follow the "
              "codebase's own conventions. Output ONLY the function/class code — no prose, no fences.")
    user = f"File `{path}` ({hint}).\nTask: {task}"
    if lesson:
        user += f"\n\nRetrieved from this codebase's history (a convention that applies here):\n\"{lesson}\""
    return api(sysmsg, user, SOLVER, max_tokens=1800)

def trial(item):
    (si, cond, _t) = item
    path, hint, task, lesson, crit, got = si
    code = solve(path, hint, task, (got or lesson) if cond == "with" else None)
    return (path, cond, judge(path, code, crit))

def run(trials=None, workers=None, verbose=True):
    """Run the real-corpus (VS Code) flywheel benchmark and return a standard summary dict."""
    trials = TRIALS if trials is None else trials
    workers = WORKERS if workers is None else workers
    if not pathlib.Path(CORPUS, "src/vs/base/common/lifecycle.ts").exists():
        raise SystemExit(f"corpus not found at {CORPUS} (set CORPUS_SRC to a vscode checkout)")
    scen, retr_ok = [], 0
    for s in SCEN:
        got = build_and_retrieve(s[0], s[3]); retr_ok += 1 if (got and s[3][:24] in got) else 0
        scen.append(s + (got,))
    n = len(SCEN)
    if verbose:
        print(f"corpus=vscode scenarios={n} trials={trials} solver={SOLVER} judge={JUDGE} · {n*2*trials} solves…")
    _items, results = run_trials(scen, trial, trials, workers)

    per, wo, wi = [], 0, 0
    for s in SCEN:
        rw = results.get((s[0], "without"), []); ri = results.get((s[0], "with"), [])
        wo += sum(rw); wi += sum(ri)
        per.append({"scenario": s[0], "without": f"{sum(rw)}/{len(rw)}", "with": f"{sum(ri)}/{len(ri)}"})
    summary = build_summary("corpus-vscode", n, trials, retr_ok, wo, wi, per)

    if verbose:
        tot = summary["samples_per_condition"]
        lw, hw = summary["without"]["ci95_pct"]; li, hi = summary["with"]["ci95_pct"]
        print(f"\n{'convention (real vscode file)':<44} {'WITHOUT':>9} {'WITH':>7}")
        print("-"*62)
        for s in SCEN:
            p = next(x for x in per if x["scenario"] == s[0])
            short = s[0].replace("src/vs/", "").replace("common/", "")
            print(f"{short:<44} {p['without']:>9} {p['with']:>7}")
        print("-"*62)
        print(f"real conventions retrieved by keel brief: {retr_ok}/{n}")
        print(f"WITHOUT keel brief: {wo}/{tot} = {100*wo/tot:.0f}%   (95% CI {lw:.0f}-{hw:.0f}%)")
        print(f"WITH    keel brief: {wi}/{tot} = {100*wi/tot:.0f}%   (95% CI {li:.0f}-{hi:.0f}%)")
        print(f"LIFT: +{100*(wi-wo)/tot:.0f} points")
    return summary


def main():
    run()


if __name__ == "__main__":
    main()
