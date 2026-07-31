#!/usr/bin/env python3
"""
Real-corpus flywheel benchmark — Django (Python). A second, different-language corpus alongside the
VS Code (TypeScript) one, to show the flywheel lift generalizes across ecosystems, not just one repo.

Same method as corpus_bench.py: 12 real, documented Django conventions, each grounded by its
frequency in a fresh django/django checkout (cached_property 278, ImproperlyConfigured 230,
format_html/mark_safe 92, reverse 63, transaction.atomic 44, force_str 38, `from django.conf import
settings` 164, gettext_lazy imports 56, …). Each convention is attached (via `keel learn`) to the
real Django file it governs; `keel brief` retrieves it. Solve WITHOUT the brief and WITH it, T trials
each; a strict dual judge (Sonnet 5) scores compliance. Solver = Opus 4.8.

Env: CORPUS_SRC (a django checkout, default ~/keel-django-demo), TRIALS (3), WORKERS (6).
"""
import json, os, subprocess, tempfile, shutil, pathlib

from bench_common import (api, judge, sh, run_trials, build_summary,
                          SOLVER, JUDGE, TRIALS, WORKERS)

CORPUS = os.environ.get("CORPUS_SRC", str(pathlib.Path.home() / "keel-django-demo"))

# (real django file, module hint, task, real grounded convention, judge compliance-criterion)
SCEN = [
 ("django/utils/translation/__init__.py", "the i18n/translation module",
  "Return the label for a 'Save changes' button shown to the user.",
  "User-facing strings must be marked translatable with gettext_lazy (imported as `_`): `_(\"...\")`; never a bare string literal for user text.",
  "wraps the user-facing text in _(...) / gettext, not a bare string literal"),
 ("django/utils/timezone.py", "the timezone utilities",
  "Implement a function that returns the current time to stamp a record.",
  "Use timezone.now() for the current time (timezone-aware); never datetime.now().",
  "uses timezone.now(); does NOT use datetime.now()"),
 ("django/utils/functional.py", "the functional utilities",
  "Add an expensive-to-compute `full_name` attribute to a class that should be computed once and reused.",
  "Cache an expensive computed attribute with @cached_property; don't recompute each access or cache by hand.",
  "decorates the attribute with @cached_property"),
 ("django/core/exceptions.py", "the core exceptions module",
  "In a backend's setup, raise an error when a required setting is missing.",
  "Raise ImproperlyConfigured for configuration/setup errors; not a bare ValueError or Exception.",
  "raises ImproperlyConfigured, not a bare ValueError/Exception"),
 ("django/shortcuts.py", "the shortcuts module",
  "Implement a view helper that fetches an Article by pk or returns 404.",
  "Fetch-or-404 with get_object_or_404(Model, ...); never a manual try/except Model.DoesNotExist raising Http404.",
  "uses get_object_or_404(...); NOT a manual try/except DoesNotExist"),
 ("django/urls/base.py", "the URL resolution module",
  "Return the URL to redirect to after creating an object (the 'article-detail' route).",
  "Build URLs with reverse('name', ...) / reverse_lazy; never hardcode a URL path string.",
  "uses reverse(...) / reverse_lazy(...); does NOT hardcode a URL path string"),
 ("django/db/transaction.py", "the DB transaction module",
  "Implement a function that debits one account and credits another (two writes).",
  "Wrap multi-write DB operations in transaction.atomic() (context manager or @transaction.atomic); never rely on autocommit for multi-step writes.",
  "wraps the writes in transaction.atomic (context manager or decorator)"),
 ("django/utils/html.py", "the HTML utilities",
  "Build an HTML link `<a href=…>label</a>` from a user-supplied url and label.",
  "Build HTML with format_html(...) / mark_safe on already-escaped pieces; never string-concatenate user data into HTML (XSS).",
  "uses format_html(...) (or mark_safe on escaped parts); does NOT concatenate user data into an HTML string"),
 ("django/utils/encoding.py", "the encoding utilities",
  "Coerce a possibly-lazy / bytes value to text for logging.",
  "Coerce a value to text with force_str(...); not a bare str(...) (force_str handles lazy objects and bytes correctly).",
  "uses force_str(...); NOT a bare str(...) coercion"),
 ("django/db/models/query.py", "the ORM QuerySet module",
  "Fetch articles and access each article.author.name in a loop, efficiently.",
  "Avoid N+1 queries: use select_related / prefetch_related when accessing related objects in a loop.",
  "uses select_related / prefetch_related to avoid per-row related queries"),
 ("django/core/checks/__init__.py", "the system checks framework",
  "Report a misconfiguration where a required app is missing from INSTALLED_APPS.",
  "Report a system/config problem via the checks framework — return a checks.Error/Warning with an id; do NOT print or raise at import time.",
  "returns a checks.Error/Warning (with an id); does NOT print or raise"),
 ("django/conf/__init__.py", "the settings module",
  "Read the configured DEFAULT_FROM_EMAIL to build a message.",
  "Read settings via `from django.conf import settings` then `settings.NAME`; never import a settings module directly or read os.environ.",
  "reads via django.conf.settings.NAME; does NOT import a settings module directly or read os.environ"),
]

def build_and_retrieve(path, lesson):
    """One keel repo per real Django file: commit real path+content, attach the convention via
    `keel learn`, retrieve via `keel brief`. One file per repo keeps retrieval clean."""
    repo = tempfile.mkdtemp(prefix="keel-django-")
    for u in (["config", "user.email", "b@e.com"], ["config", "user.name", "bench"]):
        subprocess.run(["git", "-C", repo] + u, capture_output=True)
    sh(["init"], cwd=repo)
    for u in (["config", "user.email", "b@e.com"], ["config", "user.name", "bench"]):
        subprocess.run(["git", "-C", repo] + u, capture_output=True)
    dst = pathlib.Path(repo, path)
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(pathlib.Path(CORPUS, path), dst)
    sh(["add", "-A"], cwd=repo)
    sh(["commit", "-m", f"add {path}"], cwd=repo)
    sh(["learn", "--task", path, "--lesson", lesson], cwd=repo)
    r = sh(["brief", "--file", path, "--json"], cwd=repo)
    shutil.rmtree(repo, ignore_errors=True)
    try:
        s = json.loads(r.stdout).get("sessions", [])
        return s[0]["lesson"] if s else None
    except Exception:
        return None

def solve(path, hint, task, lesson):
    sysmsg = ("You are a senior engineer working in the Django codebase (Python). Follow the "
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
    """Run the real-corpus (Django) flywheel benchmark and return a standard summary dict."""
    trials = TRIALS if trials is None else trials
    workers = WORKERS if workers is None else workers
    if not pathlib.Path(CORPUS, "django/shortcuts.py").exists():
        raise SystemExit(f"corpus not found at {CORPUS} (set CORPUS_SRC to a django checkout)")
    scen, retr_ok = [], 0
    for s in SCEN:
        got = build_and_retrieve(s[0], s[3]); retr_ok += 1 if (got and s[3][:24] in got) else 0
        scen.append(s + (got,))
    n = len(SCEN)
    if verbose:
        print(f"corpus=django scenarios={n} trials={trials} solver={SOLVER} judge={JUDGE} · {n*2*trials} solves…")
    _items, results = run_trials(scen, trial, trials, workers)

    per, wo, wi = [], 0, 0
    for s in SCEN:
        rw = results.get((s[0], "without"), []); ri = results.get((s[0], "with"), [])
        wo += sum(rw); wi += sum(ri)
        per.append({"scenario": s[0], "without": f"{sum(rw)}/{len(rw)}", "with": f"{sum(ri)}/{len(ri)}"})
    summary = build_summary("corpus-django", n, trials, retr_ok, wo, wi, per)

    if verbose:
        tot = summary["samples_per_condition"]
        lw, hw = summary["without"]["ci95_pct"]; li, hi = summary["with"]["ci95_pct"]
        print(f"\n{'convention (real django file)':<46} {'WITHOUT':>9} {'WITH':>7}")
        print("-" * 64)
        for s in SCEN:
            p = next(x for x in per if x["scenario"] == s[0])
            short = s[0].replace("django/", "")
            print(f"{short:<46} {p['without']:>9} {p['with']:>7}")
        print("-" * 64)
        print(f"real conventions retrieved by keel brief: {retr_ok}/{n}")
        print(f"WITHOUT keel brief: {wo}/{tot} = {100*wo/tot:.0f}%   (95% CI {lw:.0f}-{hw:.0f}%)")
        print(f"WITH    keel brief: {wi}/{tot} = {100*wi/tot:.0f}%   (95% CI {li:.0f}-{hi:.0f}%)")
        print(f"LIFT: +{100*(wi-wo)/tot:.0f} points")
    return summary

def main():
    run()

if __name__ == "__main__":
    main()
